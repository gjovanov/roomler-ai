use mongodb::bson::{DateTime, doc, oid::ObjectId};
use roomler_ai_config::StripeSettings;
use roomler_ai_db::models::tenant::{BillingInfo, Plan, PlanLimits, SubscriptionStatus, Tenant};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// ---- Response / DTO types ------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CheckoutResponse {
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct PortalResponse {
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct PlanInfo {
    pub id: String,
    pub name: String,
    pub price_cents: u32,
    pub features: Vec<String>,
    pub limits: PlanLimits,
}

// ---- Stripe webhook event (minimal deserialization) ----------------------

#[derive(Debug, Deserialize)]
pub struct StripeEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub data: StripeEventData,
}

#[derive(Debug, Deserialize)]
pub struct StripeEventData {
    pub object: serde_json::Value,
}

// ---- Error type ----------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum StripeError {
    #[error("Tenant not found")]
    TenantNotFound,
    #[error("No billing account for tenant")]
    NoBillingAccount,
    #[error("Invalid plan: {0}")]
    InvalidPlan(String),
    #[error("Stripe API error: {0}")]
    ApiError(String),
    #[error("Invalid webhook signature")]
    InvalidSignature,
    #[error("MongoDB error: {0}")]
    Mongo(#[from] mongodb::error::Error),
}

// ---- Service -------------------------------------------------------------

pub struct StripeService {
    settings: StripeSettings,
    client: reqwest::Client,
}

impl StripeService {
    pub fn new(settings: &StripeSettings) -> Self {
        Self {
            settings: settings.clone(),
            client: reqwest::Client::new(),
        }
    }

    // ---- Checkout --------------------------------------------------------

    pub async fn create_checkout_session(
        &self,
        db: &mongodb::Database,
        tenant_id: &ObjectId,
        plan: &str,
        email: &str,
        success_url: &str,
        cancel_url: &str,
    ) -> Result<CheckoutResponse, StripeError> {
        // Validate the plan up front, BEFORE touching Stripe — otherwise
        // an invalid plan return path runs after a Stripe API call that
        // can fail with its own error and mask the real problem (e2e:
        // empty STRIPE_SECRET_KEY → customer-create 401 → 500 Internal
        // instead of 400 Bad Request on `plan: "nonexistent"`).
        let price_id = match plan {
            "pro" => self.settings.price_pro.clone(),
            "business" => self.settings.price_business.clone(),
            _ => return Err(StripeError::InvalidPlan(plan.to_string())),
        };

        let collection = db.collection::<Tenant>(Tenant::COLLECTION);
        let tenant = collection
            .find_one(doc! { "_id": tenant_id })
            .await?
            .ok_or(StripeError::TenantNotFound)?;

        // Reuse or create Stripe customer
        let customer_id = if let Some(ref billing) = tenant.billing {
            if let Some(ref cid) = billing.customer_id {
                cid.clone()
            } else {
                self.create_customer(email, &tenant_id.to_hex()).await?
            }
        } else {
            self.create_customer(email, &tenant_id.to_hex()).await?
        };

        // Persist customer_id if it was just created
        if tenant
            .billing
            .as_ref()
            .and_then(|b| b.customer_id.as_ref())
            .is_none()
        {
            collection
                .update_one(
                    doc! { "_id": tenant_id },
                    doc! { "$set": { "billing.customer_id": &customer_id } },
                )
                .await?;
        }

        let params = [
            ("customer", customer_id.as_str()),
            ("mode", "subscription"),
            ("line_items[0][price]", price_id.as_str()),
            ("line_items[0][quantity]", "1"),
            ("success_url", success_url),
            ("cancel_url", cancel_url),
            ("metadata[tenant_id]", &tenant_id.to_hex()),
            ("metadata[plan]", plan),
        ];

        let resp: serde_json::Value = self
            .client
            .post("https://api.stripe.com/v1/checkout/sessions")
            .basic_auth(&self.settings.secret_key, None::<&str>)
            .form(&params)
            .send()
            .await
            .map_err(|e| StripeError::ApiError(e.to_string()))?
            .json()
            .await
            .map_err(|e| StripeError::ApiError(e.to_string()))?;

        if let Some(err) = resp.get("error") {
            return Err(StripeError::ApiError(
                err["message"]
                    .as_str()
                    .unwrap_or("Unknown Stripe error")
                    .to_string(),
            ));
        }

        let url = resp["url"]
            .as_str()
            .ok_or_else(|| StripeError::ApiError("No checkout URL in response".to_string()))?
            .to_string();

        Ok(CheckoutResponse { url })
    }

    // ---- Customer --------------------------------------------------------

    async fn create_customer(&self, email: &str, tenant_id: &str) -> Result<String, StripeError> {
        let params = [("email", email), ("metadata[tenant_id]", tenant_id)];

        let resp: serde_json::Value = self
            .client
            .post("https://api.stripe.com/v1/customers")
            .basic_auth(&self.settings.secret_key, None::<&str>)
            .form(&params)
            .send()
            .await
            .map_err(|e| StripeError::ApiError(e.to_string()))?
            .json()
            .await
            .map_err(|e| StripeError::ApiError(e.to_string()))?;

        if let Some(err) = resp.get("error") {
            return Err(StripeError::ApiError(
                err["message"]
                    .as_str()
                    .unwrap_or("Unknown Stripe error")
                    .to_string(),
            ));
        }

        let id = resp["id"]
            .as_str()
            .ok_or_else(|| StripeError::ApiError("No customer ID in response".to_string()))?
            .to_string();

        info!(customer_id = %id, "Created Stripe customer");
        Ok(id)
    }

    // ---- Billing portal --------------------------------------------------

    pub async fn create_portal_session(
        &self,
        db: &mongodb::Database,
        tenant_id: &ObjectId,
        return_url: &str,
    ) -> Result<PortalResponse, StripeError> {
        let collection = db.collection::<Tenant>(Tenant::COLLECTION);
        let tenant = collection
            .find_one(doc! { "_id": tenant_id })
            .await?
            .ok_or(StripeError::TenantNotFound)?;

        let customer_id = tenant
            .billing
            .as_ref()
            .and_then(|b| b.customer_id.as_ref())
            .ok_or(StripeError::NoBillingAccount)?;

        let params = [
            ("customer", customer_id.as_str()),
            ("return_url", return_url),
        ];

        let resp: serde_json::Value = self
            .client
            .post("https://api.stripe.com/v1/billing_portal/sessions")
            .basic_auth(&self.settings.secret_key, None::<&str>)
            .form(&params)
            .send()
            .await
            .map_err(|e| StripeError::ApiError(e.to_string()))?
            .json()
            .await
            .map_err(|e| StripeError::ApiError(e.to_string()))?;

        if let Some(err) = resp.get("error") {
            return Err(StripeError::ApiError(
                err["message"]
                    .as_str()
                    .unwrap_or("Unknown Stripe error")
                    .to_string(),
            ));
        }

        let url = resp["url"]
            .as_str()
            .ok_or_else(|| StripeError::ApiError("No portal URL in response".to_string()))?
            .to_string();

        Ok(PortalResponse { url })
    }

    // ---- Plans (static) --------------------------------------------------

    pub fn get_plans() -> Vec<PlanInfo> {
        vec![
            PlanInfo {
                id: "free".into(),
                name: "Free".into(),
                price_cents: 0,
                features: vec![
                    "10 members".into(),
                    "5 channels".into(),
                    "5K message history".into(),
                    "100 MB storage".into(),
                ],
                limits: Plan::Free.limits(),
            },
            PlanInfo {
                id: "pro".into(),
                name: "Pro".into(),
                price_cents: 800,
                features: vec![
                    "Unlimited members".into(),
                    "Unlimited channels".into(),
                    "Full history".into(),
                    "10 GB storage".into(),
                    "Video (10 participants)".into(),
                    "Cloud integrations".into(),
                ],
                limits: Plan::Pro.limits(),
            },
            PlanInfo {
                id: "business".into(),
                name: "Business".into(),
                price_cents: 1600,
                features: vec![
                    "Everything in Pro".into(),
                    "100 GB storage".into(),
                    "Video (100 participants)".into(),
                    "AI doc recognition".into(),
                    "Recordings".into(),
                    "Priority support".into(),
                ],
                limits: Plan::Business.limits(),
            },
        ]
    }

    // ---- Webhook processing ----------------------------------------------

    /// Verify the Stripe webhook signature using HMAC-SHA256.
    pub fn verify_signature(
        webhook_secret: &str,
        payload: &[u8],
        sig_header: &str,
    ) -> Result<(), StripeError> {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        // Parse the Stripe-Signature header: t=...,v1=...,v0=...
        let mut timestamp = None;
        let mut signatures: Vec<String> = Vec::new();

        for part in sig_header.split(',') {
            let part = part.trim();
            if let Some(t) = part.strip_prefix("t=") {
                timestamp = Some(t.to_string());
            } else if let Some(v1) = part.strip_prefix("v1=") {
                signatures.push(v1.to_string());
            }
        }

        let timestamp = timestamp.ok_or(StripeError::InvalidSignature)?;
        if signatures.is_empty() {
            return Err(StripeError::InvalidSignature);
        }

        // Build the signed payload: "{timestamp}.{body}"
        let signed_payload = format!("{timestamp}.{}", String::from_utf8_lossy(payload));

        let mut mac = Hmac::<Sha256>::new_from_slice(webhook_secret.as_bytes())
            .map_err(|_| StripeError::InvalidSignature)?;
        mac.update(signed_payload.as_bytes());
        let expected = hex::encode(mac.finalize().into_bytes());

        if signatures.iter().any(|s| s == &expected) {
            Ok(())
        } else {
            Err(StripeError::InvalidSignature)
        }
    }

    /// Handle a verified webhook event, updating tenant billing state.
    pub async fn handle_webhook_event(
        &self,
        db: &mongodb::Database,
        event: &StripeEvent,
    ) -> Result<(), StripeError> {
        let obj = &event.data.object;

        match event.event_type.as_str() {
            "checkout.session.completed" => {
                let tenant_hex = obj["metadata"]["tenant_id"].as_str().unwrap_or_default();
                let plan_str = obj["metadata"]["plan"].as_str().unwrap_or_default();
                let subscription_id = obj["subscription"].as_str().unwrap_or_default();
                let customer_id = obj["customer"].as_str().unwrap_or_default();

                if tenant_hex.is_empty() {
                    warn!("checkout.session.completed missing tenant_id metadata");
                    return Ok(());
                }

                let tenant_id = ObjectId::parse_str(tenant_hex)
                    .map_err(|_| StripeError::ApiError("Invalid tenant_id in metadata".into()))?;

                let plan = match plan_str {
                    "pro" => Plan::Pro,
                    "business" => Plan::Business,
                    _ => Plan::Free,
                };

                let collection = db.collection::<Tenant>(Tenant::COLLECTION);
                collection
                    .update_one(
                        doc! { "_id": tenant_id },
                        doc! {
                            "$set": {
                                "plan": bson::to_bson(&plan).unwrap_or_default(),
                                "billing": bson::to_bson(&BillingInfo {
                                    customer_id: Some(customer_id.to_string()),
                                    subscription_id: Some(subscription_id.to_string()),
                                    current_period_end: None,
                                    status: SubscriptionStatus::Active,
                                    cancel_at_period_end: false,
                                }).unwrap_or_default(),
                                "updated_at": DateTime::now(),
                            }
                        },
                    )
                    .await?;

                info!(
                    tenant_id = %tenant_hex,
                    plan = %plan_str,
                    "Tenant plan upgraded via checkout"
                );
            }

            "customer.subscription.updated" => {
                let subscription_id = obj["id"].as_str().unwrap_or_default();
                let status = obj["status"].as_str().unwrap_or_default();
                let cancel_at_period_end = obj["cancel_at_period_end"].as_bool().unwrap_or(false);
                let current_period_end = obj["current_period_end"].as_i64();

                let sub_status = match status {
                    "active" => SubscriptionStatus::Active,
                    "past_due" => SubscriptionStatus::PastDue,
                    "canceled" => SubscriptionStatus::Canceled,
                    "trialing" => SubscriptionStatus::Trialing,
                    "incomplete" => SubscriptionStatus::Incomplete,
                    _ => SubscriptionStatus::Active,
                };

                let period_end = current_period_end.map(|ts| DateTime::from_millis(ts * 1000));

                let collection = db.collection::<Tenant>(Tenant::COLLECTION);
                let mut update = doc! {
                    "billing.status": bson::to_bson(&sub_status).unwrap_or_default(),
                    "billing.cancel_at_period_end": cancel_at_period_end,
                    "updated_at": DateTime::now(),
                };
                if let Some(pe) = period_end {
                    update.insert("billing.current_period_end", pe);
                }

                collection
                    .update_one(
                        doc! { "billing.subscription_id": subscription_id },
                        doc! { "$set": update },
                    )
                    .await?;

                info!(
                    subscription_id = %subscription_id,
                    status = %status,
                    "Subscription updated"
                );
            }

            "customer.subscription.deleted" => {
                let subscription_id = obj["id"].as_str().unwrap_or_default();

                let collection = db.collection::<Tenant>(Tenant::COLLECTION);
                collection
                    .update_one(
                        doc! { "billing.subscription_id": subscription_id },
                        doc! {
                            "$set": {
                                "plan": bson::to_bson(&Plan::Free).unwrap_or_default(),
                                "billing.status": bson::to_bson(&SubscriptionStatus::Canceled).unwrap_or_default(),
                                "billing.cancel_at_period_end": false,
                                "updated_at": DateTime::now(),
                            }
                        },
                    )
                    .await?;

                info!(
                    subscription_id = %subscription_id,
                    "Subscription deleted, reverted to Free plan"
                );
            }

            "invoice.payment_failed" => {
                let subscription_id = obj["subscription"].as_str().unwrap_or_default();

                let collection = db.collection::<Tenant>(Tenant::COLLECTION);
                collection
                    .update_one(
                        doc! { "billing.subscription_id": subscription_id },
                        doc! {
                            "$set": {
                                "billing.status": bson::to_bson(&SubscriptionStatus::PastDue).unwrap_or_default(),
                                "updated_at": DateTime::now(),
                            }
                        },
                    )
                    .await?;

                warn!(
                    subscription_id = %subscription_id,
                    "Invoice payment failed"
                );
            }

            other => {
                info!(event_type = %other, "Unhandled Stripe webhook event");
            }
        }

        Ok(())
    }
}
