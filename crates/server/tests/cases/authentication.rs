use crate::support::*;
use agent_computer_core::identity::{OrganizationId, PrincipalId};
use agent_computer_store::auth::ServiceScope;
use axum::http::{HeaderValue, StatusCode};

#[tokio::test]
async fn authentication_precedes_body_parsing_and_requires_exact_scope() {
    let service = Service::new().await;
    for token in [None, Some("forged")] {
        let (status, value) = service
            .send(validate_request(token, "not json".into()))
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(value["code"], "unauthenticated");
    }
    let manage = service
        .issue("manager", &[ServiceScope::DefinitionsManage])
        .await;
    assert_eq!(
        service
            .send(validate_request(
                Some(manage.expose_token()),
                document().to_string()
            ))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let reader = service
        .issue("reader", &[ServiceScope::DefinitionsValidate])
        .await;
    let (status, body) = service
        .send(validate_request(
            Some(reader.expose_token()),
            document().to_string(),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["valid"], true);
    assert_eq!(body["scope"], "static");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM declaration_versions")
        .fetch_one(&service.database.pool)
        .await
        .unwrap();
    assert_eq!(count, 0); // Static validation does not apply or even persist a document.
    let mut duplicate = validate_request(Some(reader.expose_token()), document().to_string());
    duplicate
        .headers_mut()
        .append("authorization", HeaderValue::from_static("Bearer forged"));
    assert_eq!(service.send(duplicate).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn spoofed_identity_browser_origin_and_revoked_credentials_are_rejected() {
    let service = Service::new().await;
    let first = service
        .issue("reader", &[ServiceScope::DefinitionsValidate])
        .await;
    let second = service
        .issue("reader", &[ServiceScope::DefinitionsValidate])
        .await;
    let mut forged = document();
    forged["principal_id"] = "a-private-principal-value".into();
    let (status, body) = service
        .send(validate_request(
            Some(first.expose_token()),
            forged.to_string(),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!body.to_string().contains("a-private-principal-value"));
    let mut browser = validate_request(Some(first.expose_token()), document().to_string());
    browser.headers_mut().insert(
        "origin",
        HeaderValue::from_static("https://attacker.invalid"),
    );
    assert_eq!(service.send(browser).await.0, StatusCode::FORBIDDEN);
    let organization = OrganizationId::new("acme").unwrap();
    service
        .store
        .revoke_credential(&organization, first.id())
        .await
        .unwrap();
    assert_eq!(
        service
            .send(validate_request(
                Some(first.expose_token()),
                document().to_string()
            ))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        service
            .send(validate_request(
                Some(second.expose_token()),
                document().to_string()
            ))
            .await
            .0,
        StatusCode::OK
    );
    service
        .store
        .disable_principal(&organization, &PrincipalId::new("reader").unwrap())
        .await
        .unwrap();
    assert_eq!(
        service
            .send(validate_request(
                Some(second.expose_token()),
                document().to_string()
            ))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}
