use crate::fixtures::test_app::TestApp;
use serde_json::Value;

#[tokio::test]
async fn oauth_redirect_google_returns_302() {
    let app = TestApp::spawn_with_oauth().await;

    let resp = app
        .client
        .get(app.url("/api/oauth/google"))
        .send()
        .await
        .unwrap();

    // Should redirect to Google's OAuth consent page
    assert_eq!(resp.status().as_u16(), 307);
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(location.starts_with("https://accounts.google.com/o/oauth2/v2/auth"));
    assert!(location.contains("client_id=test-google-id"));
    assert!(location.contains("redirect_uri="));
    assert!(location.contains("scope=email+profile"));
}

#[tokio::test]
async fn oauth_redirect_github_returns_302() {
    let app = TestApp::spawn_with_oauth().await;

    let resp = app
        .client
        .get(app.url("/api/oauth/github"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 307);
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(location.starts_with("https://github.com/login/oauth/authorize"));
    assert!(location.contains("client_id=test-github-id"));
}

#[tokio::test]
async fn oauth_redirect_facebook_returns_302() {
    let app = TestApp::spawn_with_oauth().await;

    let resp = app
        .client
        .get(app.url("/api/oauth/facebook"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 307);
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(location.starts_with("https://www.facebook.com/v18.0/dialog/oauth"));
}

#[tokio::test]
async fn oauth_redirect_linkedin_returns_302() {
    let app = TestApp::spawn_with_oauth().await;

    let resp = app
        .client
        .get(app.url("/api/oauth/linkedin"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 307);
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(location.starts_with("https://www.linkedin.com/oauth/v2/authorization"));
}

#[tokio::test]
async fn oauth_redirect_microsoft_returns_302() {
    let app = TestApp::spawn_with_oauth().await;

    let resp = app
        .client
        .get(app.url("/api/oauth/microsoft"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 307);
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(location.starts_with("https://login.microsoftonline.com/common/oauth2/v2.0/authorize"));
}

#[tokio::test]
async fn oauth_redirect_unknown_provider_returns_400() {
    let app = TestApp::spawn_with_oauth().await;

    let resp = app
        .client
        .get(app.url("/api/oauth/unknown"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn oauth_callback_without_code_returns_error() {
    let app = TestApp::spawn_with_oauth().await;

    let resp = app
        .client
        .get(app.url("/api/oauth/callback/google"))
        .send()
        .await
        .unwrap();

    // Missing query params → 400
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn oauth_user_dao_find_or_create_new_user() {
    let app = TestApp::spawn().await;

    let user = app
        .db
        .collection::<bson::Document>("users")
        .count_documents(bson::doc! {})
        .await
        .unwrap();
    assert_eq!(user, 0);

    // Create user via OAuth DAO
    let dao = roomler_ai_services::dao::user::UserDao::new(&app.db);
    let user = dao
        .find_or_create_by_oauth(
            "google",
            "google-123",
            "oauth@test.com",
            "OAuth User",
            Some("https://example.com/avatar.jpg"),
            true,
        )
        .await
        .unwrap();

    assert_eq!(user.email, "oauth@test.com");
    assert_eq!(user.display_name, "OAuth User");
    assert_eq!(
        user.avatar,
        Some("https://example.com/avatar.jpg".to_string())
    );
    assert!(user.is_verified);
    assert!(user.password_hash.is_none());
    assert_eq!(user.oauth_providers.len(), 1);
    assert_eq!(user.oauth_providers[0].provider, "google");
    assert_eq!(user.oauth_providers[0].provider_id, "google-123");
}

#[tokio::test]
async fn oauth_user_dao_links_existing_user() {
    let app = TestApp::spawn().await;

    // First, register a regular user
    let resp = app
        .client
        .post(app.url("/api/auth/register"))
        .json(&serde_json::json!({
            "email": "existing@test.com",
            "username": "existing",
            "display_name": "Existing User",
            "password": "Password123!",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);

    // Now use OAuth with the same email
    let dao = roomler_ai_services::dao::user::UserDao::new(&app.db);
    let user = dao
        .find_or_create_by_oauth(
            "github",
            "gh-456",
            "existing@test.com",
            "GitHub User",
            None,
            true,
        )
        .await
        .unwrap();

    // Should be the same user, with OAuth linked
    assert_eq!(user.email, "existing@test.com");
    assert_eq!(user.username, "existing"); // keeps original username
    assert_eq!(user.oauth_providers.len(), 1);
    assert_eq!(user.oauth_providers[0].provider, "github");
    assert_eq!(user.oauth_providers[0].provider_id, "gh-456");
}

#[tokio::test]
async fn oauth_user_dao_does_not_duplicate_provider() {
    let app = TestApp::spawn().await;

    let dao = roomler_ai_services::dao::user::UserDao::new(&app.db);

    // Create via OAuth
    dao.find_or_create_by_oauth("google", "g-789", "nodupe@test.com", "No Dupe", None, true)
        .await
        .unwrap();

    // Call again with same provider/id
    let user = dao
        .find_or_create_by_oauth("google", "g-789", "nodupe@test.com", "No Dupe", None, true)
        .await
        .unwrap();

    // Should still have only 1 oauth provider, not 2
    assert_eq!(user.oauth_providers.len(), 1);
}

/// nOAuth: an identity whose email the provider did NOT verify must get
/// its OWN account, never inherit one that happens to share the address.
/// Microsoft's multi-tenant endpoint returns a tenant-settable `mail`, so
/// treating it as an account key was an account-takeover path.
#[tokio::test]
async fn unverified_oauth_email_never_links_into_an_existing_account() {
    let app = TestApp::spawn().await;
    let dao = roomler_ai_services::dao::user::UserDao::new(&app.db);

    // Victim signs up normally.
    let victim = dao
        .create(
            "victim@corp.example".to_string(),
            "victim".to_string(),
            "Victim".to_string(),
            "hash".to_string(),
        )
        .await
        .unwrap();

    // Attacker signs in via a provider asserting the SAME address with no
    // verification claim — must NOT resolve to the victim.
    let attacker = dao
        .find_or_create_by_oauth(
            "microsoft",
            "ms-attacker-oid",
            "victim@corp.example",
            "Not The Victim",
            None,
            false,
        )
        .await
        .unwrap();
    assert_ne!(
        attacker.id.unwrap(),
        victim.id.unwrap(),
        "unverified email must not take over the existing account"
    );
    assert!(
        attacker
            .oauth_providers
            .iter()
            .any(|p| p.provider == "microsoft")
    );

    // The victim's account is untouched (no provider grafted onto it).
    let victim_after = dao.find_by_email("victim@corp.example").await.unwrap();
    assert_eq!(victim_after.id.unwrap(), victim.id.unwrap());
    assert!(victim_after.oauth_providers.is_empty());

    // Same provider identity signs in again → its own account, stably.
    let again = dao
        .find_or_create_by_oauth(
            "microsoft",
            "ms-attacker-oid",
            "victim@corp.example",
            "Not The Victim",
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(again.id.unwrap(), attacker.id.unwrap());

    // A VERIFIED provider email still links, as before.
    let linked = dao
        .find_or_create_by_oauth(
            "google",
            "g-verified",
            "victim@corp.example",
            "Victim",
            None,
            true,
        )
        .await
        .unwrap();
    assert_eq!(linked.id.unwrap(), victim.id.unwrap());
}
