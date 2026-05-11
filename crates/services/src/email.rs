use std::sync::Arc;

use lettre::{
    AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
    message::{Mailbox, header::ContentType},
};
use roomler_ai_config::EmailSettings;
use serde::Serialize;
use tracing::{info, warn};

/// Outbound email service. Two backends are supported and selected
/// at construction time based on `EmailSettings`:
///   - `SendGrid` (HTTP POST to api.sendgrid.com) — prod default,
///     active when `email.api_key` is non-empty.
///   - `Smtp` (plaintext SMTP via lettre) — used by the e2e overlay
///     to capture mail in Mailpit; active when `email.api_key` is
///     empty AND `email.smtp_host` + `email.smtp_port` are both set.
///
/// `from_settings` returns `None` when neither backend is configured;
/// `AppState` then leaves `state.email = None` and the register
/// handler silently skips email sends.
#[derive(Clone)]
pub struct EmailService {
    backend: EmailBackend,
    from_email: String,
    from_name: String,
}

#[derive(Clone)]
enum EmailBackend {
    SendGrid {
        client: reqwest::Client,
        api_key: String,
    },
    Smtp {
        transport: Arc<AsyncSmtpTransport<Tokio1Executor>>,
        endpoint: String,
    },
}

#[derive(Debug, Serialize)]
struct SendGridRequest {
    personalizations: Vec<Personalization>,
    from: EmailAddress,
    subject: String,
    content: Vec<Content>,
}

#[derive(Debug, Serialize)]
struct Personalization {
    to: Vec<EmailAddress>,
}

#[derive(Debug, Serialize)]
struct EmailAddress {
    email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Debug, Serialize)]
struct Content {
    #[serde(rename = "type")]
    content_type: String,
    value: String,
}

impl EmailService {
    /// Build an `EmailService` from `EmailSettings`. Picks SendGrid
    /// when `api_key` is non-empty (prod), otherwise picks SMTP when
    /// `smtp_host` + `smtp_port` are both set (e2e Mailpit). Returns
    /// `None` when neither path is configured.
    pub fn from_settings(settings: &EmailSettings) -> Option<Self> {
        if !settings.api_key.is_empty() {
            return Some(Self {
                backend: EmailBackend::SendGrid {
                    client: reqwest::Client::new(),
                    api_key: settings.api_key.clone(),
                },
                from_email: settings.from_email.clone(),
                from_name: settings.from_name.clone(),
            });
        }
        if let (Some(host), Some(port)) = (settings.smtp_host.as_ref(), settings.smtp_port) {
            // `builder_dangerous` skips TLS — appropriate for the
            // in-cluster Mailpit target which doesn't terminate TLS.
            // For prod-grade SMTP delivery (future enhancement),
            // switch to `starttls_relay` + credentials.
            let transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host.as_str())
                .port(port)
                .build();
            let endpoint = format!("{}:{}", host, port);
            return Some(Self {
                backend: EmailBackend::Smtp {
                    transport: Arc::new(transport),
                    endpoint,
                },
                from_email: settings.from_email.clone(),
                from_name: settings.from_name.clone(),
            });
        }
        None
    }

    /// Low-level send — used by every `send_*` helper below. Routes
    /// through whichever backend was selected at construction.
    pub async fn send(&self, to_email: &str, subject: &str, html_body: &str) -> anyhow::Result<()> {
        match &self.backend {
            EmailBackend::SendGrid { client, api_key } => {
                let request = SendGridRequest {
                    personalizations: vec![Personalization {
                        to: vec![EmailAddress {
                            email: to_email.to_string(),
                            name: None,
                        }],
                    }],
                    from: EmailAddress {
                        email: self.from_email.clone(),
                        name: Some(self.from_name.clone()),
                    },
                    subject: subject.to_string(),
                    content: vec![Content {
                        content_type: "text/html".to_string(),
                        value: html_body.to_string(),
                    }],
                };

                let resp = client
                    .post("https://api.sendgrid.com/v3/mail/send")
                    .bearer_auth(api_key)
                    .json(&request)
                    .send()
                    .await?;

                if resp.status().is_success() {
                    info!(to = to_email, subject, "Email sent via SendGrid");
                    Ok(())
                } else {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    warn!(to = to_email, %status, body, "SendGrid email failed");
                    anyhow::bail!("SendGrid error {}: {}", status, body)
                }
            }
            EmailBackend::Smtp {
                transport,
                endpoint,
            } => {
                let from_addr: lettre::Address = self.from_email.parse().map_err(|e| {
                    anyhow::anyhow!("Invalid from_email '{}': {}", self.from_email, e)
                })?;
                let from_mb = Mailbox::new(
                    if self.from_name.is_empty() {
                        None
                    } else {
                        Some(self.from_name.clone())
                    },
                    from_addr,
                );

                let to_addr: lettre::Address = to_email
                    .parse()
                    .map_err(|e| anyhow::anyhow!("Invalid to_email '{}': {}", to_email, e))?;
                let to_mb = Mailbox::new(None, to_addr);

                let email = lettre::Message::builder()
                    .from(from_mb)
                    .to(to_mb)
                    .subject(subject)
                    .header(ContentType::TEXT_HTML)
                    .body(html_body.to_string())
                    .map_err(|e| anyhow::anyhow!("Failed to build SMTP message: {}", e))?;

                transport
                    .send(email)
                    .await
                    .map_err(|e| anyhow::anyhow!("SMTP send to {} failed: {}", endpoint, e))?;
                info!(to = to_email, subject, endpoint, "Email sent via SMTP");
                Ok(())
            }
        }
    }

    /// Send an invite email with a link to accept the invitation.
    pub async fn send_invite(
        &self,
        to_email: &str,
        inviter_name: &str,
        tenant_name: &str,
        invite_url: &str,
    ) -> anyhow::Result<()> {
        let subject = format!("You're invited to join {} on Roomler", tenant_name);
        let html = format!(
            r#"<div style="font-family: sans-serif; max-width: 600px; margin: 0 auto;">
<h2>You're invited!</h2>
<p><strong>{inviter}</strong> has invited you to join <strong>{tenant}</strong> on Roomler.</p>
<p style="margin: 24px 0;">
  <a href="{url}" style="background: #1976d2; color: #fff; padding: 12px 24px; border-radius: 6px; text-decoration: none; font-weight: bold;">
    Accept Invitation
  </a>
</p>
<p style="color: #666; font-size: 13px;">
  Or copy this link: <a href="{url}">{url}</a>
</p>
<p style="color: #999; font-size: 12px; margin-top: 32px;">— The Roomler Team</p>
</div>"#,
            inviter = inviter_name,
            tenant = tenant_name,
            url = invite_url,
        );
        self.send(to_email, &subject, &html).await
    }

    /// Send a mention notification email.
    pub async fn send_mention_notification(
        &self,
        to_email: &str,
        mentioner_name: &str,
        room_name: &str,
        message_preview: &str,
        link_url: &str,
    ) -> anyhow::Result<()> {
        let subject = format!("{} mentioned you in #{}", mentioner_name, room_name);
        let html = format!(
            r#"<div style="font-family: sans-serif; max-width: 600px; margin: 0 auto;">
<h2>You were mentioned</h2>
<p><strong>{mentioner}</strong> mentioned you in <strong>#{room}</strong>:</p>
<blockquote style="border-left: 3px solid #1976d2; padding: 8px 12px; margin: 16px 0; color: #333; background: #f5f5f5; border-radius: 4px;">
  {preview}
</blockquote>
<p style="margin: 24px 0;">
  <a href="{url}" style="background: #1976d2; color: #fff; padding: 12px 24px; border-radius: 6px; text-decoration: none; font-weight: bold;">
    View Message
  </a>
</p>
<p style="color: #999; font-size: 12px; margin-top: 32px;">— The Roomler Team</p>
</div>"#,
            mentioner = mentioner_name,
            room = room_name,
            preview = message_preview,
            url = link_url,
        );
        self.send(to_email, &subject, &html).await
    }

    /// Send an account activation email with a verification link.
    pub async fn send_activation(
        &self,
        to_email: &str,
        display_name: &str,
        activation_url: &str,
        ttl_minutes: u64,
    ) -> anyhow::Result<()> {
        let subject = "Activate your Roomler account".to_string();
        let html = format!(
            r#"<div style="font-family: sans-serif; max-width: 600px; margin: 0 auto;">
<h2>Welcome, {name}!</h2>
<p>Please activate your account by clicking the button below. This link expires in {ttl} minutes.</p>
<p style="margin: 32px 0;">
  <a href="{url}" style="background: #1976d2; color: #fff; padding: 12px 24px; border-radius: 6px; text-decoration: none; font-weight: bold;">
    Activate Account
  </a>
</p>
<p style="color: #666; font-size: 13px;">Or copy this link: <a href="{url}">{url}</a></p>
<p style="color: #999; font-size: 12px; margin-top: 32px;">If you did not create an account, please ignore this email.</p>
</div>"#,
            name = display_name,
            url = activation_url,
            ttl = ttl_minutes,
        );
        self.send(to_email, &subject, &html).await
    }

    /// Send account activation success email.
    pub async fn send_activation_success(
        &self,
        to_email: &str,
        display_name: &str,
        login_url: &str,
    ) -> anyhow::Result<()> {
        let subject = "Your Roomler account is active".to_string();
        let html = format!(
            r#"<div style="font-family: sans-serif; max-width: 600px; margin: 0 auto;">
<h2>Account activated, {name}!</h2>
<p>Your Roomler account is now active. You can <a href="{url}">sign in here</a>.</p>
<p style="color: #999; font-size: 12px; margin-top: 32px;">— The Roomler Team</p>
</div>"#,
            name = display_name,
            url = login_url,
        );
        self.send(to_email, &subject, &html).await
    }

    /// Send a welcome email after registration.
    pub async fn send_welcome(
        &self,
        to_email: &str,
        display_name: &str,
        app_url: &str,
    ) -> anyhow::Result<()> {
        let subject = "Welcome to Roomler!".to_string();
        let html = format!(
            r#"<div style="font-family: sans-serif; max-width: 600px; margin: 0 auto;">
<h2>Welcome, {name}!</h2>
<p>Your Roomler account is ready. Start collaborating with your team.</p>
<p style="margin: 24px 0;">
  <a href="{url}" style="background: #1976d2; color: #fff; padding: 12px 24px; border-radius: 6px; text-decoration: none; font-weight: bold;">
    Get Started
  </a>
</p>
<p style="color: #999; font-size: 12px; margin-top: 32px;">— The Roomler Team</p>
</div>"#,
            name = display_name,
            url = app_url,
        );
        self.send(to_email, &subject, &html).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(api_key: &str, smtp_host: Option<&str>, smtp_port: Option<u16>) -> EmailSettings {
        EmailSettings {
            api_key: api_key.to_string(),
            from_email: "noreply@test.local".to_string(),
            from_name: "Test".to_string(),
            activation_token_ttl_minutes: 5,
            smtp_host: smtp_host.map(|s| s.to_string()),
            smtp_port,
        }
    }

    #[test]
    fn from_settings_returns_none_when_unconfigured() {
        let s = settings("", None, None);
        assert!(EmailService::from_settings(&s).is_none());
    }

    #[test]
    fn from_settings_returns_sendgrid_when_api_key_set() {
        let s = settings("sg.key", None, None);
        let svc = EmailService::from_settings(&s).expect("expected Some");
        assert!(matches!(svc.backend, EmailBackend::SendGrid { .. }));
    }

    #[test]
    fn from_settings_returns_smtp_when_host_and_port_set() {
        let s = settings("", Some("mailpit.local"), Some(1025));
        let svc = EmailService::from_settings(&s).expect("expected Some");
        match svc.backend {
            EmailBackend::Smtp { endpoint, .. } => {
                assert_eq!(endpoint, "mailpit.local:1025");
            }
            _ => panic!("expected SMTP backend"),
        }
    }

    #[test]
    fn from_settings_prefers_sendgrid_over_smtp() {
        let s = settings("sg.key", Some("mailpit.local"), Some(1025));
        let svc = EmailService::from_settings(&s).expect("expected Some");
        assert!(matches!(svc.backend, EmailBackend::SendGrid { .. }));
    }

    #[test]
    fn from_settings_returns_none_when_smtp_partially_configured() {
        let host_only = settings("", Some("mailpit.local"), None);
        assert!(EmailService::from_settings(&host_only).is_none());

        let port_only = settings("", None, Some(1025));
        assert!(EmailService::from_settings(&port_only).is_none());
    }
}
