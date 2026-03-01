use tracing::{info, warn};
use web_push::{
    ContentEncoding, IsahcWebPushClient, SubscriptionInfo, VapidSignatureBuilder, WebPushClient,
    WebPushMessageBuilder,
};

#[derive(Debug, Clone)]
pub struct PushService {
    vapid_private_key: Vec<u8>,
    contact: String,
}

impl PushService {
    pub fn new(vapid_private_key_pem: &str, contact: String) -> anyhow::Result<Self> {
        // Decode PEM to raw bytes for VAPID signing
        let key_bytes = vapid_private_key_pem.as_bytes().to_vec();
        Ok(Self {
            vapid_private_key: key_bytes,
            contact,
        })
    }

    pub async fn send(
        &self,
        endpoint: &str,
        auth: &str,
        p256dh: &str,
        title: &str,
        body: &str,
        link: Option<&str>,
    ) -> anyhow::Result<()> {
        let subscription = SubscriptionInfo::new(endpoint, p256dh, auth);

        let payload = serde_json::json!({
            "title": title,
            "body": body,
            "url": link,
        });
        let payload_str = serde_json::to_string(&payload)?;

        let mut sig_builder = VapidSignatureBuilder::from_pem(
            &mut self.vapid_private_key.as_slice(),
            &subscription,
        )?;
        sig_builder.add_claim("sub", serde_json::Value::String(self.contact.clone()));

        let signature = sig_builder.build()?;

        let mut builder = WebPushMessageBuilder::new(&subscription);
        builder.set_payload(ContentEncoding::Aes128Gcm, payload_str.as_bytes());
        builder.set_vapid_signature(signature);

        let message = builder.build()?;
        let client = IsahcWebPushClient::new()?;

        match client.send(message).await {
            Ok(_) => {
                info!(endpoint, title, "Push notification sent");
                Ok(())
            }
            Err(e) => {
                warn!(endpoint, %e, "Push notification failed");
                Err(e.into())
            }
        }
    }
}
