// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! RED (#3403), removed in the next commit: one scoped default outside the
//! helper for each `disallowed-methods` entry of the root clippy.toml, so a
//! single clippy run shows every entry refusing its call.

use tracing::instrument::WithSubscriber;
use tracing::subscriber::NoSubscriber;
use tracing_subscriber::util::SubscriberInitExt;

#[test]
fn a_scoped_default_outside_the_helper_fails_clippy() {
    let none = tracing::Dispatch::none();
    let _first = tracing::subscriber::set_default(NoSubscriber::default());
    tracing::subscriber::with_default(NoSubscriber::default(), || {});
    let _third = tracing::dispatcher::set_default(&none);
    tracing::dispatcher::with_default(&none, || {});
    let _fifth = std::future::ready(()).with_subscriber(none.clone());
    let _sixth = NoSubscriber::default().set_default();
}
