use super::*;
use crate::agent::agent_profiles::{AgentProfile, InvalidProfile, ProfileCatalog, ProfileSettings};

#[derive(serde::Serialize)]
pub(super) struct ProfileList {
    profiles: Vec<AgentProfile>,
    drafts: Vec<AgentProfile>,
    #[serde(flatten)]
    catalog: ProfileCatalog,
}

pub(super) async fn list(
    Extension(state): Extension<WebState>,
) -> Result<Json<ProfileList>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let catalog = catalog(chat).await?;
    let (profiles, drafts) = chat
        .sessions
        .list_agent_profiles(&catalog)
        .await
        .map_err(internal)?;
    Ok(Json(ProfileList {
        profiles,
        drafts,
        catalog,
    }))
}

pub(super) async fn save(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
    Json(settings): Json<ProfileSettings>,
) -> Result<StatusCode, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    crate::agent::agent_profiles::validate_writable_id(&id).map_err(input_error)?;
    let catalog = catalog(chat).await?;
    chat.sessions
        .save_agent_profile(&id, settings, &catalog)
        .await
        .map_err(input_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn delete(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
) -> Result<StatusCode, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    if !chat
        .sessions
        .delete_agent_profile(&id)
        .await
        .map_err(input_error)?
    {
        return Err(error(StatusCode::NOT_FOUND, "Agent profile not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn catalog(chat: &Chat) -> Result<ProfileCatalog, Response> {
    let manager = chat.runner.subagents().cloned().ok_or_else(|| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Agent profiles are unavailable",
        )
    })?;
    Ok(
        tokio::task::spawn_blocking(move || manager.profile_choices())
            .await
            .map_err(|err| internal(err.into()))?
            .map_err(internal)?
            .into(),
    )
}

fn input_error(reason: anyhow::Error) -> Response {
    match reason.downcast_ref::<InvalidProfile>() {
        Some(invalid) => error(StatusCode::BAD_REQUEST, invalid.0),
        None => internal(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn agent_profile_routes_require_auth_and_protect_bundled_roles() {
        use tower::ServiceExt;

        let data = tempfile::tempdir().unwrap();
        let token = AuthStore::new(data.path())
            .test_session(Identity::Owner, chrono::Utc::now().timestamp())
            .unwrap()
            .unwrap();
        let chat = crate::web::chat_tests::chat(data.path()).await;
        let router = routes(
            Settings {
                trusted_proxies: vec![],
                listen: "127.0.0.1:0".parse().unwrap(),
                base_url: "http://localhost/".parse().unwrap(),
            },
            data.path(),
            Some(chat),
        );
        let request = |method: Method, suffix: &str, signed: bool, origin: &str| {
            let mut builder = Request::builder()
                .method(method)
                .uri(format!("/api/chat/agent-profiles{suffix}"))
                .header("origin", origin)
                .header("content-type", "application/json");
            if signed {
                builder = builder.header("cookie", format!("kodrik_web_session={token}"));
            }
            builder
                .body(axum::body::Body::from(
                    json!({"name":"Personal", "instructions":"Review the text", "model":null,
                            "tools":[], "skills":[]})
                    .to_string(),
                ))
                .unwrap()
        };
        for (method, suffix, signed, origin, status) in [
            (
                Method::GET,
                "",
                false,
                "http://localhost",
                StatusCode::UNAUTHORIZED,
            ),
            (
                Method::PUT,
                "/personal",
                true,
                "https://example.invalid",
                StatusCode::FORBIDDEN,
            ),
            (
                Method::PUT,
                "/builtin-reviewer",
                true,
                "http://localhost",
                StatusCode::BAD_REQUEST,
            ),
            (
                Method::DELETE,
                "/builtin-reviewer",
                true,
                "http://localhost",
                StatusCode::BAD_REQUEST,
            ),
            (
                Method::PUT,
                "/personal",
                true,
                "http://localhost",
                StatusCode::NO_CONTENT,
            ),
        ] {
            let response = router
                .clone()
                .oneshot(request(method, suffix, signed, origin))
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{suffix}");
        }
        let response = router
            .clone()
            .oneshot(request(Method::GET, "", true, "http://localhost"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let list: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let profiles = list["profiles"].as_array().unwrap();
        assert!(
            profiles
                .iter()
                .any(|profile| profile["id"] == "builtin-reviewer" && profile["builtin"] == true)
        );
        assert!(profiles.iter().any(|profile| profile["id"] == "personal"
            && profile["builtin"] == false
            && profile["name"] == "Personal"));
        assert_eq!(list["drafts"], json!([]));
        let response = router
            .oneshot(request(
                Method::DELETE,
                "/personal",
                true,
                "http://localhost",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
}
