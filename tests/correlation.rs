use axum::{
    Router,
    body::Body,
    extract::{Extension, Request},
    http::{HeaderMap, StatusCode},
    middleware,
    routing::get,
};
use tower_http::request_id::RequestId;
use underclass::correlation;

#[tokio::test]
async fn shared_identity_survives_handlers_and_error_responses() {
    let app = Router::new()
        .route(
            "/ok",
            get(
                |Extension(id): Extension<RequestId>, request: Request| async move {
                    assert_eq!(id.header_value(), &request.headers()["x-request-id"]);
                    let mut response = axum::response::Response::new(Body::from("ok"));
                    response.headers_mut().insert(
                        "x-request-id",
                        "provider-id-must-not-replace-correlation".parse().unwrap(),
                    );
                    response
                },
            ),
        )
        .route("/denied", get(|| async { StatusCode::UNAUTHORIZED }))
        .layer(middleware::from_fn(correlation::middleware));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    let supplied = uuid::Uuid::new_v4().to_string();
    for (path, status) in [("/ok", 200), ("/denied", 401), ("/missing", 404)] {
        let response = client
            .get(format!("http://{address}{path}"))
            .header("x-request-id", &supplied)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(response.headers()["x-request-id"], supplied);
    }
    for malformed in [
        "client-controlled-text",
        "00000000-0000-0000-0000-000000000000",
        "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
    ] {
        let response = client
            .get(format!("http://{address}/denied"))
            .header("x-request-id", malformed)
            .send()
            .await
            .unwrap();
        let id = response.headers()["x-request-id"].to_str().unwrap();
        assert_ne!(id, malformed);
        assert_eq!(
            uuid::Uuid::parse_str(id).unwrap().get_version(),
            Some(uuid::Version::Random)
        );
    }
    let mut headers = HeaderMap::new();
    headers.append("x-request-id", supplied.parse().unwrap());
    headers.append("x-request-id", supplied.parse().unwrap());
    let response = client
        .get(format!("http://{address}/ok"))
        .headers(headers)
        .send()
        .await
        .unwrap();
    assert_ne!(response.headers()["x-request-id"], supplied);
    let response = client
        .get(format!("http://{address}/missing"))
        .send()
        .await
        .unwrap();
    assert!(correlation::incoming_id(response.headers()).is_some());
    task.abort();
}
