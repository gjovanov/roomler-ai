use serde::Serialize;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct EmailService {
    client: reqwest::Client,
    api_key: String,
    from_email: String,
    from_name: String,
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
    pub fn new(api_key: String, from_email: String, from_name: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            from_email,
            from_name,
        }
    }

    pub async fn send(&self, to_email: &str, subject: &str, html_body: &str) -> anyhow::Result<()> {
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

        let resp = self
            .client
            .post("https://api.sendgrid.com/v3/mail/send")
            .bearer_auth(&self.api_key)
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
