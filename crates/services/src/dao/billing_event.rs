// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P4 — `billing_events`: what Stripe billed, one row per webhook
//! event of the six kinds §3g names (and one per paid invoice the operator's
//! backfill read). The `saas` module owns the collection and its indexes;
//! this DAO sits below both its writer (the Stripe webhook, the backfill)
//! and its readers (the host's revenue view, the funnel's `paid` step, the
//! digest), which is why it lives here.
//!
//! * **Billed, not listed.** `amount_cents` is what the invoice billed;
//!   `recurring_cents` the recurring part of it (the subscription lines
//!   that are not prorations, after coupons, before tax) — the basis of
//!   billed MRR. Subscription events carry no amount: `null`, never 0.
//! * **Recorded once.** A webhook row's `event_id` is unique (Stripe's own
//!   retry of an event writes nothing new); a paid invoice is unique by
//!   `(kind, stripe_id)`, so the webhook and the backfill can both see it
//!   and it is still one row.
//! * **With the org.** Rows carry the tenant and go when the org goes
//!   ([`BillingEventDao::purge_tenant`]); the monthly totals frozen into
//!   `growth_daily` carry no tenant and stay.

use std::collections::HashMap;

use bson::{Bson, DateTime, Document, doc, oid::ObjectId};
use futures::TryStreamExt;
use mongodb::{Collection, Database};
use serde::{Deserialize, Serialize};

use super::base::DaoResult;

pub const BILLING_EVENTS: &str = "billing_events";
/// The most rows one read loads — far above this deployment's volume; a
/// bound, not a page.
pub const MAX_ROWS: i64 = 200_000;

/// The six Stripe events §3g records. The stored spelling is the
/// snake_case variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingKind {
    SubscriptionCreated,
    SubscriptionUpdated,
    SubscriptionDeleted,
    InvoicePaid,
    InvoicePaymentFailed,
    TrialWillEnd,
}

impl BillingKind {
    pub const ALL: [BillingKind; 6] = [
        BillingKind::SubscriptionCreated,
        BillingKind::SubscriptionUpdated,
        BillingKind::SubscriptionDeleted,
        BillingKind::InvoicePaid,
        BillingKind::InvoicePaymentFailed,
        BillingKind::TrialWillEnd,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            BillingKind::SubscriptionCreated => "subscription_created",
            BillingKind::SubscriptionUpdated => "subscription_updated",
            BillingKind::SubscriptionDeleted => "subscription_deleted",
            BillingKind::InvoicePaid => "invoice_paid",
            BillingKind::InvoicePaymentFailed => "invoice_payment_failed",
            BillingKind::TrialWillEnd => "trial_will_end",
        }
    }
}

/// Where a row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingSource {
    /// Stripe's signed webhook.
    Webhook,
    /// The operator's one-off read of Stripe's invoice list.
    Backfill,
}

/// One row. Amounts are in the currency's smallest unit (cents).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BillingEvent {
    #[serde(rename = "_id")]
    pub id: ObjectId,
    pub tenant_id: ObjectId,
    pub kind: BillingKind,
    /// `pro` · `business` from the price id (`stripe.price_*`); `None` for a
    /// price this server does not sell.
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub seats: Option<i64>,
    /// Invoice kinds: what the invoice billed (`amount_paid`, or
    /// `amount_due` for a failed payment). `None` for a subscription event.
    #[serde(default)]
    pub amount_cents: Option<i64>,
    /// Invoice kinds: the recurring part — subscription lines that are not
    /// prorations, after discounts, before tax.
    #[serde(default)]
    pub recurring_cents: Option<i64>,
    /// The price's billing interval in months (1 monthly, 12 yearly), when
    /// the invoice named it.
    #[serde(default)]
    pub interval_months: Option<f64>,
    /// Lowercase ISO 4217.
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub period_start: Option<DateTime>,
    #[serde(default)]
    pub period_end: Option<DateTime>,
    /// The invoice (`in_…`) or subscription (`sub_…`) id.
    pub stripe_id: String,
    #[serde(default)]
    pub subscription_id: Option<String>,
    /// Stripe's status of the object (`active`, `trialing`, `paid`, …).
    #[serde(default)]
    pub status: Option<String>,
    /// An invoice's `billing_reason`.
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub cancel_at_period_end: Option<bool>,
    /// The Stripe event (`evt_…`); absent on a backfilled row — absent, not
    /// null, so the sparse unique index skips it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    pub source: BillingSource,
    /// When it happened: an invoice's `paid_at`, a deletion's `ended_at`,
    /// else the event's `created`.
    pub ts: DateTime,
    pub recorded_at: DateTime,
}

/// What a record call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Recorded {
    Created,
    /// The event, or the paid invoice, is already on record.
    Duplicate,
}

pub struct BillingEventDao {
    db: Database,
}

fn is_duplicate(e: &mongodb::error::Error) -> bool {
    matches!(
        *e.kind,
        mongodb::error::ErrorKind::Write(mongodb::error::WriteFailure::WriteError(ref we))
            if we.code == 11000
    ) || matches!(
        *e.kind,
        mongodb::error::ErrorKind::Command(ref ce) if ce.code == 11000
    )
}

impl BillingEventDao {
    pub fn new(db: &Database) -> Self {
        Self { db: db.clone() }
    }

    fn rows(&self) -> Collection<BillingEvent> {
        self.db.collection::<BillingEvent>(BILLING_EVENTS)
    }

    /// Record one row. A paid invoice is upserted on `(kind, stripe_id)` —
    /// the webhook and the backfill meet on it; anything else is inserted,
    /// its `event_id` the uniqueness. Either way a second sighting is
    /// [`Recorded::Duplicate`] and changes nothing.
    pub async fn record(&self, row: &BillingEvent) -> DaoResult<Recorded> {
        if row.kind == BillingKind::InvoicePaid {
            let mut set = bson::to_document(row)?;
            set.remove("_id");
            let r = self
                .db
                .collection::<Document>(BILLING_EVENTS)
                .update_one(
                    doc! { "kind": BillingKind::InvoicePaid.as_str(), "stripe_id": &row.stripe_id },
                    doc! { "$setOnInsert": set },
                )
                .upsert(true)
                .await;
            return match r {
                Ok(r) if r.upserted_id.is_some() => Ok(Recorded::Created),
                Ok(_) => Ok(Recorded::Duplicate),
                Err(e) if is_duplicate(&e) => Ok(Recorded::Duplicate),
                Err(e) => Err(e.into()),
            };
        }
        match self.rows().insert_one(row).await {
            Ok(_) => Ok(Recorded::Created),
            Err(e) if is_duplicate(&e) => Ok(Recorded::Duplicate),
            Err(e) => Err(e.into()),
        }
    }

    /// Every row, oldest first, minus the excluded orgs; only the given orgs
    /// when `tenants` is `Some`. Bounded by [`MAX_ROWS`].
    pub async fn list(
        &self,
        tenants: Option<&[ObjectId]>,
        excluded: &[ObjectId],
    ) -> DaoResult<Vec<BillingEvent>> {
        let tenant_filter = match tenants {
            Some(ids) => doc! { "$in": ids, "$nin": excluded },
            None => doc! { "$nin": excluded },
        };
        let docs: Vec<Document> = self
            .db
            .collection::<Document>(BILLING_EVENTS)
            .find(doc! { "tenant_id": tenant_filter })
            .sort(doc! { "ts": 1 })
            .limit(MAX_ROWS)
            .await?
            .try_collect()
            .await?;
        // Lenient per row: one malformed row costs itself, never the view.
        Ok(docs
            .into_iter()
            .filter_map(|d| match bson::from_document::<BillingEvent>(d) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!(error = %e, "billing_events: skipping an unreadable row");
                    None
                }
            })
            .collect())
    }

    /// Each org's first invoice paid with an amount above 0 (ms) — the
    /// funnel's `paid` step.
    pub async fn first_paid(&self, tenants: &[ObjectId]) -> DaoResult<HashMap<ObjectId, i64>> {
        if tenants.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<Document> = self
            .db
            .collection::<Document>(BILLING_EVENTS)
            .aggregate(vec![
                doc! { "$match": {
                    "tenant_id": { "$in": tenants },
                    "kind": BillingKind::InvoicePaid.as_str(),
                    "amount_cents": { "$gt": 0_i64 },
                } },
                doc! { "$group": { "_id": "$tenant_id", "first": { "$min": "$ts" } } },
            ])
            .await?
            .try_collect()
            .await?;
        Ok(rows
            .iter()
            .filter_map(|d| {
                Some((
                    d.get_object_id("_id").ok()?,
                    d.get_datetime("first").ok()?.timestamp_millis(),
                ))
            })
            .collect())
    }

    /// When the record begins — the oldest row's instant, any org: before
    /// it nothing was recorded, so billed figures there are not measured
    /// (never zero). `None` while there is no row at all.
    pub async fn first_ts(&self) -> DaoResult<Option<DateTime>> {
        let first = self
            .db
            .collection::<Document>(BILLING_EVENTS)
            .find_one(doc! {})
            .sort(doc! { "ts": 1 })
            .projection(doc! { "ts": 1 })
            .await?;
        Ok(first.and_then(|d| d.get_datetime("ts").ok().copied()))
    }

    /// Everything about one org, removed (an org deletion's call). The
    /// frozen monthly totals carry no tenant and stay.
    pub async fn purge_tenant(&self, tenant_id: ObjectId) -> DaoResult<u64> {
        Ok(self
            .db
            .collection::<Document>(BILLING_EVENTS)
            .delete_many(doc! { "tenant_id": Bson::ObjectId(tenant_id) })
            .await?
            .deleted_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stored spellings are persisted keys: renaming one would make
    /// every existing row unreadable.
    #[test]
    fn kind_spellings_are_locked() {
        for k in BillingKind::ALL {
            assert_eq!(
                serde_json::to_string(&k).unwrap(),
                format!("\"{}\"", k.as_str())
            );
        }
        assert_eq!(BillingKind::InvoicePaid.as_str(), "invoice_paid");
        assert_eq!(
            serde_json::to_string(&BillingSource::Backfill).unwrap(),
            "\"backfill\""
        );
    }

    /// A backfilled row has NO `event_id` key (the sparse unique index
    /// skips it), and a webhook row round-trips.
    #[test]
    fn a_backfilled_row_has_no_event_id_key() {
        let row = BillingEvent {
            id: ObjectId::new(),
            tenant_id: ObjectId::new(),
            kind: BillingKind::InvoicePaid,
            plan: Some("pro".into()),
            seats: Some(3),
            amount_cents: Some(1920),
            recurring_cents: Some(1920),
            interval_months: Some(1.0),
            currency: Some("usd".into()),
            period_start: Some(DateTime::now()),
            period_end: Some(DateTime::now()),
            stripe_id: "in_1".into(),
            subscription_id: Some("sub_1".into()),
            status: Some("paid".into()),
            reason: None,
            cancel_at_period_end: None,
            event_id: None,
            source: BillingSource::Backfill,
            ts: DateTime::now(),
            recorded_at: DateTime::now(),
        };
        let doc = bson::to_document(&row).unwrap();
        assert!(!doc.contains_key("event_id"));
        let back: BillingEvent = bson::from_document(doc).unwrap();
        assert_eq!(back, row);
        let mut w = row.clone();
        w.event_id = Some("evt_1".into());
        w.source = BillingSource::Webhook;
        let doc = bson::to_document(&w).unwrap();
        assert_eq!(doc.get_str("event_id").unwrap(), "evt_1");
        assert_eq!(doc.get_str("kind").unwrap(), "invoice_paid");
    }
}
