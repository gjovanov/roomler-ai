// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P1d — the digest's email bodies, HTML and plain text, rendered
//! from the STORED sections (never from a second query: what was mailed is
//! what `GET …/digest` answers).
//!
//! Both bodies come from one intermediate model ([`Block`]) so the two can
//! not disagree; the HTML writer escapes every string (org names, UTM
//! values and referrer hosts are client-chosen text). The rules the text
//! follows:
//!
//! * every count sits beside its 7- and 28-day context (`fig`);
//! * a rate is shown only when its denominator reaches `SMALL_N`
//!   ([`pct_if_enough`]) — below it, counts only;
//! * a `not_measured` section reads "not measured — arrives with Pn" (or
//!   the reason), never 0;
//! * the footer says how many users and orgs were excluded, and that the
//!   digest carries no personal addresses.

use roomler_ai_services::{dao::growth_digest::DigestDoc, newsletter::escape_html};

use super::digest::{CampaignGoals, Figure, FigureF, Measured, NotMeasured, SMALL_N, Sections};

/// `Roomler daily digest — YYYY-MM-DD`.
pub fn subject(day: &str) -> String {
    format!("Roomler daily digest — {day}")
}

/// `num / den` as a percentage, only once `den` reaches [`SMALL_N`].
pub fn pct_if_enough(num: i64, den: i64) -> Option<f64> {
    (den >= SMALL_N).then(|| (num as f64 / den as f64 * 1000.0).round() / 10.0)
}

/// The render model.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Heading(String),
    /// `label: value`
    Line(String, String),
    /// A table: headers, then rows of cells.
    Table(Vec<String>, Vec<Vec<String>>),
    Note(String),
}

pub fn fig(f: &Figure) -> String {
    format!("{} (7d {} · 28d {})", f.day, f.d7, f.d28)
}

pub fn figf(f: &FigureF) -> String {
    format!("{:.1} (7d {:.1} · 28d {:.1})", f.day, f.d7, f.d28)
}

pub fn not_measured(n: &NotMeasured) -> String {
    match &n.arrives_with {
        Some(p) => format!("not measured — arrives with {p} ({})", n.not_measured),
        None => format!("not measured — {}", n.not_measured),
    }
}

fn measured<T>(m: &Measured<T>, show: impl FnOnce(&T) -> String) -> String {
    match m {
        Measured::Value(v) => show(v),
        Measured::NotMeasured(n) => not_measured(n),
    }
}

fn opt(s: &Option<String>) -> String {
    s.clone().unwrap_or_else(|| "—".to_string())
}

fn money(cents: f64) -> String {
    format!("{:.2}", cents / 100.0)
}

/// The sections as blocks, in the order the operator reads them.
pub fn blocks(doc: &DigestDoc, s: &Sections) -> Vec<Block> {
    use Block::*;
    let mut out = Vec::new();

    out.push(Heading("Headline".into()));
    out.push(Line("Sign-ups".into(), fig(&s.headline.signups)));
    out.push(Line("Orgs created".into(), fig(&s.headline.orgs_created)));
    out.push(Line(
        "Devices enrolled".into(),
        measured(&s.headline.devices_enrolled, fig),
    ));
    out.push(Line(
        "Active orgs".into(),
        measured(&s.headline.active_orgs, |a| {
            format!(
                "{} (7d {} · 28d {}) — basis: {}",
                a.day,
                a.d7,
                a.d28,
                a.basis.join(" + ")
            )
        }),
    ));
    out.push(Line(
        "Coverage".into(),
        measured(&s.headline.coverage, |c| {
            let ratio = match c.ratio {
                Some(r) => format!(", {:.2}", r),
                None => format!(" (no ratio below n = {})", c.n_floor),
            };
            format!(
                "{} purestat sign-up goals vs {} server sign-ups{ratio}",
                c.purestat_signup_goals, c.server_signups
            )
        }),
    ));

    out.push(Heading("Sign-ups".into()));
    out.push(Line("Total".into(), fig(&s.signups.total)));
    out.push(Line("With attribution".into(), fig(&s.signups.attributed)));
    if !s.signups.by_provider.is_empty() {
        out.push(Table(
            vec!["Provider".into(), "Sign-ups".into()],
            s.signups
                .by_provider
                .iter()
                .map(|k| vec![k.key.clone(), k.count.to_string()])
                .collect(),
        ));
    }
    if !s.signups.by_source.is_empty() {
        out.push(Table(
            vec!["Source".into(), "Sign-ups".into()],
            s.signups
                .by_source
                .iter()
                .map(|k| vec![k.key.clone(), k.count.to_string()])
                .collect(),
        ));
    }
    match &s.signups.rows {
        Some(rows) if !rows.is_empty() => out.push(Table(
            vec![
                "Provider".into(),
                "Source".into(),
                "Medium".into(),
                "Campaign".into(),
                "Content".into(),
                "Landing".into(),
                "Referrer".into(),
                "Self-reported".into(),
            ],
            rows.iter()
                .map(|r| {
                    vec![
                        r.provider.clone(),
                        opt(&r.source),
                        opt(&r.medium),
                        opt(&r.campaign),
                        opt(&r.content),
                        opt(&r.landing_path),
                        opt(&r.referrer_host),
                        opt(&r.self_reported),
                    ]
                })
                .collect(),
        )),
        Some(_) => {}
        None => out.push(Note(
            "More than the listed ceiling of sign-ups — aggregated by source above.".into(),
        )),
    }

    out.push(Heading("Orgs and devices".into()));
    out.push(Line(
        "Orgs created".into(),
        fig(&s.orgs_devices.orgs_created),
    ));
    out.push(Line(
        "Devices enrolled".into(),
        measured(&s.orgs_devices.devices_enrolled, fig),
    ));
    out.push(Line(
        "Orgs enrolling their first device".into(),
        measured(&s.orgs_devices.orgs_first_device, fig),
    ));
    match &s.orgs_devices.devices_by_os {
        Measured::Value(rows) if !rows.is_empty() => out.push(Table(
            vec!["OS".into(), "Enrolled".into()],
            rows.iter()
                .map(|k| vec![k.key.clone(), k.count.to_string()])
                .collect(),
        )),
        Measured::Value(_) => {}
        Measured::NotMeasured(n) => out.push(Line("By OS".into(), not_measured(n))),
    }
    out.push(Line(
        "By install method".into(),
        not_measured(&s.orgs_devices.by_install_method),
    ));

    out.push(Heading("Usage".into()));
    out.push(Line(
        "Remote-desktop sessions".into(),
        measured(&s.usage.remote, |r| fig(&r.sessions_started)),
    ));
    out.push(Line(
        "Remote-desktop minutes".into(),
        measured(&s.usage.remote, |r| figf(&r.session_minutes)),
    ));
    out.push(Line(
        "Calls".into(),
        measured(&s.usage.calls, |c| fig(&c.calls)),
    ));
    out.push(Line(
        "Participant-minutes".into(),
        measured(&s.usage.calls, |c| figf(&c.participant_minutes)),
    ));
    out.push(Line(
        "Active orgs".into(),
        measured(&s.usage.active_orgs, |a| {
            format!("{} (7d {} · 28d {})", a.day, a.d7, a.d28)
        }),
    ));
    match &s.usage.top_orgs {
        Measured::Value(rows) if !rows.is_empty() => out.push(Table(
            vec!["Org".into(), "Minutes".into(), "Sessions".into()],
            rows.iter()
                .map(|o| {
                    vec![
                        o.name.clone(),
                        format!("{:.1}", o.minutes),
                        o.sessions.to_string(),
                    ]
                })
                .collect(),
        )),
        Measured::Value(_) => out.push(Note("No remote-desktop minutes that day.".into())),
        Measured::NotMeasured(n) => out.push(Line("Top orgs".into(), not_measured(n))),
    }

    out.push(Heading("Website".into()));
    match &s.website {
        Measured::Value(w) => {
            out.push(Line("Visitors".into(), fig(&w.visitors)));
            out.push(Line("Visits".into(), fig(&w.visits)));
            out.push(Line("Pageviews".into(), fig(&w.pageviews)));
            if !w.channels.is_empty() {
                out.push(Table(
                    vec!["Channel".into(), "Visitors".into()],
                    w.channels
                        .iter()
                        .map(|k| vec![k.key.clone(), k.count.to_string()])
                        .collect(),
                ));
            }
            if !w.top_referrers.is_empty() {
                out.push(Table(
                    vec!["Referrer".into(), "Channel".into(), "Visitors".into()],
                    w.top_referrers
                        .iter()
                        .map(|r| vec![r.host.clone(), r.channel.clone(), r.visitors.to_string()])
                        .collect(),
                ));
            }
            if !w.top_paths.is_empty() {
                out.push(Table(
                    vec!["Path".into(), "Visitors".into(), "Pageviews".into()],
                    w.top_paths
                        .iter()
                        .map(|p| {
                            vec![
                                p.path.clone(),
                                p.visitors.to_string(),
                                p.pageviews.to_string(),
                            ]
                        })
                        .collect(),
                ));
            }
        }
        Measured::NotMeasured(n) => out.push(Note(not_measured(n))),
    }

    out.push(Heading("Blog".into()));
    match &s.blog {
        Measured::Value(b) => {
            if b.posts.is_empty() {
                out.push(Note("No blog post was viewed that day.".into()));
            } else {
                out.push(Table(
                    vec![
                        "Post".into(),
                        "Visitors".into(),
                        "Pageviews".into(),
                        "Reads".into(),
                        "Median read (s)".into(),
                    ],
                    b.posts
                        .iter()
                        .map(|p| {
                            vec![
                                p.path.clone(),
                                p.visitors.to_string(),
                                p.pageviews.to_string(),
                                p.reads.map(|r| r.to_string()).unwrap_or_else(|| "—".into()),
                                p.median_read_s
                                    .map(|r| format!("{r:.0}"))
                                    .unwrap_or_else(|| "—".into()),
                            ]
                        })
                        .collect(),
                ));
            }
            if !b.top_sources.is_empty() {
                out.push(Table(
                    vec!["Blog source".into(), "Visitors".into()],
                    b.top_sources
                        .iter()
                        .map(|k| vec![k.key.clone(), k.count.to_string()])
                        .collect(),
                ));
            }
        }
        Measured::NotMeasured(n) => out.push(Note(not_measured(n))),
    }

    out.push(Heading("Campaigns".into()));
    match &s.campaigns {
        Measured::Value(c) if c.rows.is_empty() => out.push(Note(
            "No campaign traffic or campaign sign-up that day.".into(),
        )),
        Measured::Value(c) => {
            // P2 — a goal the site does not define: the visitors are real,
            // the conversion columns are not measured, and the note says
            // which goal (never a 0).
            if let Some(n) = &c.conversions {
                out.push(Note(format!("Goal conversions: {}", not_measured(n))));
            }
            let goal = |m: &Measured<CampaignGoals>, pick: fn(&CampaignGoals) -> i64| match m {
                Measured::Value(g) => pick(g).to_string(),
                Measured::NotMeasured(_) => "—".to_string(),
            };
            out.push(Table(
                vec![
                    "Source".into(),
                    "Medium".into(),
                    "Campaign".into(),
                    "Visitors".into(),
                    "Pageviews".into(),
                    "Goal: signup".into(),
                    "Goal: subscribe".into(),
                    "Goal: install-copy".into(),
                    "Goal: github".into(),
                    "Server sign-ups".into(),
                ],
                c.rows
                    .iter()
                    .map(|r| {
                        vec![
                            r.source.clone(),
                            r.medium.clone(),
                            r.campaign.clone(),
                            r.visitors.to_string(),
                            r.pageviews.to_string(),
                            goal(&r.goals, |g| g.signup),
                            goal(&r.goals, |g| g.subscribe),
                            goal(&r.goals, |g| g.install_copy),
                            goal(&r.goals, |g| g.github_outbound),
                            r.signups.to_string(),
                        ]
                    })
                    .collect(),
            ));
        }
        Measured::NotMeasured(n) => out.push(Note(not_measured(n))),
    }

    out.push(Heading("Newsletter".into()));
    match &s.newsletter {
        Measured::Value(n) => {
            out.push(Line("New subscribers".into(), fig(&n.new)));
            out.push(Line("Confirmed".into(), fig(&n.confirmed)));
            out.push(Line("Unsubscribed".into(), fig(&n.unsubscribed)));
            if let Some(p) = pct_if_enough(n.confirmed.d28, n.new.d28) {
                out.push(Line("Confirm rate (28d)".into(), format!("{p:.1} %")));
            }
            if !n.by_source.is_empty() {
                out.push(Table(
                    vec![
                        "Source".into(),
                        "New".into(),
                        "Confirmed".into(),
                        "Unsubscribed".into(),
                    ],
                    n.by_source
                        .iter()
                        .map(|r| {
                            vec![
                                r.source.clone(),
                                r.new.to_string(),
                                r.confirmed.to_string(),
                                r.unsubscribed.to_string(),
                            ]
                        })
                        .collect(),
                ));
            }
            if n.issues.is_empty() {
                out.push(Note("No issue was sent that day.".into()));
            } else {
                out.push(Table(
                    vec![
                        "Issue".into(),
                        "Subject".into(),
                        "Sent at".into(),
                        "Recipients".into(),
                        "Accepted".into(),
                        "Failed".into(),
                        "Suppressed".into(),
                        "Stale".into(),
                    ],
                    n.issues
                        .iter()
                        .map(|i| {
                            vec![
                                i.slug.clone(),
                                i.subject.clone(),
                                i.sent_at.clone(),
                                i.counts.total.to_string(),
                                i.counts.sent.to_string(),
                                i.counts.failed.to_string(),
                                i.counts.suppressed.to_string(),
                                i.counts.stale.to_string(),
                            ]
                        })
                        .collect(),
                ));
            }
            out.push(Line("Delivery".into(), not_measured(&n.delivery)));
        }
        Measured::NotMeasured(n) => out.push(Note(not_measured(n))),
    }

    out.push(Heading("Revenue".into()));
    if s.revenue.orgs_by_plan_status.is_empty() {
        out.push(Note("No orgs.".into()));
    } else {
        out.push(Table(
            vec!["Plan".into(), "Subscription".into(), "Orgs".into()],
            s.revenue
                .orgs_by_plan_status
                .iter()
                .map(|r| vec![r.plan.clone(), r.status.clone(), r.orgs.to_string()])
                .collect(),
        ));
    }
    out.push(Line(
        "MRR (ESTIMATE)".into(),
        format!("{} — {}", money(s.revenue.mrr.cents), s.revenue.mrr.basis),
    ));
    out.push(Line("Billed".into(), not_measured(&s.revenue.billed)));

    out.push(Heading("Quality".into()));
    match &s.quality.remote {
        Measured::Value(q) => {
            out.push(Line("Sessions ended".into(), fig(&q.ended)));
            out.push(Line("Hang-ups".into(), fig(&q.hangups)));
            out.push(Line("Refusals".into(), fig(&q.refusals)));
            out.push(Line("Failures".into(), fig(&q.failures)));
            if let Some(p) = pct_if_enough(q.failures.d28, q.ended.d28) {
                out.push(Line("Failure share (28d)".into(), format!("{p:.1} %")));
            }
            if !q.by_end_reason.is_empty() {
                out.push(Table(
                    vec!["End reason".into(), "Class".into(), "Sessions".into()],
                    q.by_end_reason
                        .iter()
                        .map(|r| vec![r.reason.clone(), r.class.clone(), r.count.to_string()])
                        .collect(),
                ));
            }
        }
        Measured::NotMeasured(n) => out.push(Line("Remote sessions".into(), not_measured(n))),
    }
    match &s.quality.crashes {
        Measured::Value(c) => {
            out.push(Line("Agent crashes".into(), fig(&c.total)));
            if !c.by_agent_version.is_empty() {
                out.push(Table(
                    vec!["Agent version".into(), "Crashes".into()],
                    c.by_agent_version
                        .iter()
                        .map(|k| vec![k.key.clone(), k.count.to_string()])
                        .collect(),
                ));
            }
        }
        Measured::NotMeasured(n) => out.push(Line("Agent crashes".into(), not_measured(n))),
    }

    out.push(Heading("Not yet measured".into()));
    out.push(Line("Install".into(), not_measured(&s.install)));
    out.push(Line("Social reach".into(), not_measured(&s.social)));
    out.push(Line("Search".into(), not_measured(&s.search)));

    out.push(Note(format!(
        "Excluded from every number above: {} users and {} orgs (platform admins, the orgs they own, \
         and the explicit exclusion list). This digest carries no personal addresses, names or IPs. \
         Day {} (UTC), attempt {}.",
        doc.excluded.users, doc.excluded.tenants, doc.day, doc.attempts
    )));
    out
}

/// The HTML body. Every string escaped; inline styles only (mail clients).
pub fn render_html(doc: &DigestDoc, s: &Sections) -> String {
    let mut html = String::with_capacity(16 * 1024);
    html.push_str(
        "<div style=\"font-family: sans-serif; max-width: 860px; margin: 0 auto; color: #222;\">",
    );
    html.push_str(&format!(
        "<h2 style=\"margin-bottom: 4px;\">Roomler daily digest — {}</h2>\
         <p style=\"color: #666; margin-top: 0;\">Every count is the day, then the trailing 7 and 28 days. \
         Rates appear only above n = {}.</p>",
        escape_html(&doc.day),
        SMALL_N
    ));
    for b in blocks(doc, s) {
        match b {
            Block::Heading(h) => html.push_str(&format!(
                "<h3 style=\"margin: 20px 0 6px; border-bottom: 1px solid #ddd;\">{}</h3>",
                escape_html(&h)
            )),
            Block::Line(k, v) => html.push_str(&format!(
                "<div style=\"margin: 2px 0;\"><strong>{}</strong>: {}</div>",
                escape_html(&k),
                escape_html(&v)
            )),
            Block::Table(headers, rows) => {
                html.push_str(
                    "<table style=\"border-collapse: collapse; margin: 6px 0; font-size: 13px;\"><thead><tr>",
                );
                for h in &headers {
                    html.push_str(&format!(
                        "<th style=\"text-align: left; padding: 2px 8px; border-bottom: 1px solid #999;\">{}</th>",
                        escape_html(h)
                    ));
                }
                html.push_str("</tr></thead><tbody>");
                for row in &rows {
                    html.push_str("<tr>");
                    for cell in row {
                        html.push_str(&format!(
                            "<td style=\"padding: 2px 8px; border-bottom: 1px solid #eee;\">{}</td>",
                            escape_html(cell)
                        ));
                    }
                    html.push_str("</tr>");
                }
                html.push_str("</tbody></table>");
            }
            Block::Note(n) => html.push_str(&format!(
                "<p style=\"color: #666; font-size: 13px;\">{}</p>",
                escape_html(&n)
            )),
        }
    }
    html.push_str(
        "<p style=\"color: #999; font-size: 12px; margin-top: 24px;\">— Roomler growth</p></div>",
    );
    html
}

/// The plain-text body: the same blocks, as text.
pub fn render_text(doc: &DigestDoc, s: &Sections) -> String {
    let mut text = String::with_capacity(8 * 1024);
    text.push_str(&format!(
        "Roomler daily digest — {}\nEvery count is the day, then the trailing 7 and 28 days. Rates appear only above n = {}.\n",
        doc.day, SMALL_N
    ));
    for b in blocks(doc, s) {
        match b {
            Block::Heading(h) => text.push_str(&format!("\n== {h} ==\n")),
            Block::Line(k, v) => text.push_str(&format!("{k}: {v}\n")),
            Block::Table(headers, rows) => {
                text.push_str(&format!("  {}\n", headers.join(" | ")));
                for row in rows {
                    text.push_str(&format!("  {}\n", row.join(" | ")));
                }
            }
            Block::Note(n) => text.push_str(&format!("{n}\n")),
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::growth::digest::{
        ActiveOrgs, Headline, KeyCount, MrrEstimate, OrgsDevices, Quality, Revenue, SignupRow,
        Signups, TopOrg, Usage,
    };
    use roomler_ai_services::dao::growth_digest::{DigestStatus, ExcludedCounts};

    /// A minimal day with one of everything the renderer branches on, and a
    /// planted org name that reads as markup.
    fn sample() -> (DigestDoc, Sections) {
        let f = |day, d7, d28| Figure { day, d7, d28 };
        let doc = DigestDoc {
            day: "2026-10-13".into(),
            status: DigestStatus::Stored,
            version: 1,
            claimed_at: bson::DateTime::now(),
            computed_at: None,
            sent_at: None,
            attempts: 1,
            error: None,
            recipients: 2,
            excluded: ExcludedCounts {
                users: 1,
                tenants: 3,
            },
            sections: None,
        };
        let s = Sections {
            headline: Headline {
                signups: f(2, 9, 31),
                orgs_created: f(1, 4, 12),
                devices_enrolled: Measured::Value(f(3, 7, 20)),
                active_orgs: Measured::Value(ActiveOrgs {
                    day: 2,
                    d7: 5,
                    d28: 9,
                    basis: vec!["remote".into(), "conference".into()],
                }),
                coverage: Measured::NotMeasured(NotMeasured::now("purestat key not configured")),
            },
            signups: Signups {
                total: f(2, 9, 31),
                attributed: f(1, 3, 10),
                by_provider: vec![KeyCount {
                    key: "email".into(),
                    count: 2,
                }],
                by_source: vec![KeyCount {
                    key: "youtube".into(),
                    count: 1,
                }],
                rows: Some(vec![SignupRow {
                    provider: "email".into(),
                    source: Some("youtube".into()),
                    medium: Some("video".into()),
                    campaign: Some("<b>c1</b>".into()),
                    content: None,
                    landing_path: None,
                    referrer_host: None,
                    self_reported: None,
                }]),
            },
            orgs_devices: OrgsDevices {
                orgs_created: f(1, 4, 12),
                devices_enrolled: Measured::Value(f(3, 7, 20)),
                devices_by_os: Measured::Value(vec![KeyCount {
                    key: "linux".into(),
                    count: 3,
                }]),
                orgs_first_device: Measured::Value(f(1, 2, 5)),
                by_install_method: NotMeasured::later("devices by install method", "P3"),
            },
            usage: Usage {
                remote: Measured::unmounted("remote"),
                calls: Measured::unmounted("conference"),
                active_orgs: Measured::unmounted("remote and conference"),
                top_orgs: Measured::Value(vec![TopOrg {
                    tenant_id: "x".into(),
                    name: "<script>alert(1)</script> Org".into(),
                    minutes: 12.5,
                    sessions: 2,
                }]),
            },
            website: Measured::NotMeasured(NotMeasured::now("purestat key not configured")),
            blog: Measured::NotMeasured(NotMeasured::now("purestat key not configured")),
            campaigns: Measured::NotMeasured(NotMeasured::now("purestat key not configured")),
            newsletter: Measured::unmounted("saas"),
            revenue: Revenue {
                orgs_by_plan_status: vec![],
                mrr: MrrEstimate {
                    cents: 1600.0,
                    estimate: true,
                    basis: "list price × seats".into(),
                },
                billed: NotMeasured::later("billed revenue", "P4"),
            },
            quality: Quality {
                remote: Measured::unmounted("remote"),
                crashes: Measured::unmounted("fleet"),
            },
            install: NotMeasured::later("install", "P3"),
            social: NotMeasured::later("social", "P8"),
            search: NotMeasured::later("search", "P7"),
        };
        (doc, s)
    }

    /// Below n = 30 there is no percentage; at 30 there is.
    #[test]
    fn rates_need_thirty() {
        assert_eq!(pct_if_enough(5, 10), None);
        assert_eq!(pct_if_enough(29, 29), None);
        assert_eq!(pct_if_enough(15, 30), Some(50.0));
        assert_eq!(pct_if_enough(1, 300), Some(0.3));
        assert_eq!(pct_if_enough(0, 0), None);
    }

    #[test]
    fn not_measured_reads_as_such_never_as_zero() {
        let (doc, s) = sample();
        let text = render_text(&doc, &s);
        assert!(text.contains("Website"));
        assert!(text.contains("not measured — purestat key not configured"));
        assert!(text.contains("not measured — arrives with P3 (devices by install method)"));
        assert!(text.contains("not measured — remote module not mounted"));
        assert!(text.contains("Remote-desktop minutes: not measured"));
        // The figure carries its context, and the footer its exclusions.
        assert!(text.contains("Sign-ups: 2 (7d 9 · 28d 31)"));
        assert!(text.contains("Excluded from every number above: 1 users and 3 orgs"));
        assert!(text.contains("no personal addresses"));
        assert!(text.contains("MRR (ESTIMATE): 16.00"));
        assert_eq!(subject("2026-10-13"), "Roomler daily digest — 2026-10-13");
    }

    #[test]
    fn html_escapes_every_client_chosen_string() {
        let (doc, s) = sample();
        let html = render_html(&doc, &s);
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt; Org"));
        assert!(html.contains("&lt;b&gt;c1&lt;/b&gt;"));
        assert!(html.contains("<h3"));
        assert!(!html.contains('@'), "no address anywhere in a digest");
    }
}
