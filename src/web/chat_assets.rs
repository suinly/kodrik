use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

static ASSETS: include_dir::Dir<'_> = include_dir::include_dir!("$OUT_DIR/chat-assets");

pub(crate) fn available() -> bool {
    ASSETS.get_file("index.html").is_some()
}

pub(super) fn index(prefix: &str) -> Response {
    page(prefix, "")
}

pub(super) fn home(prefix: &str, logged_in: bool) -> Response {
    let data = serde_json::json!({"prefix":prefix,"loggedIn":logged_in})
        .to_string()
        .replace('<', "\\u003c");
    page(
        prefix,
        &format!("<script id=\"kodrik-home\" type=\"application/json\">{data}</script>"),
    )
}

pub(super) fn login(prefix: &str, return_to: &str, error: &str) -> Response {
    let data = serde_json::json!({"returnTo":return_to,"error":error})
        .to_string()
        .replace('<', "\\u003c");
    page(
        prefix,
        &format!("<script id=\"kodrik-login\" type=\"application/json\">{data}</script>"),
    )
}

pub(super) fn document(settings: &super::Settings, slug: &str) -> anyhow::Result<Response> {
    let url = settings.url(&format!("pages/{slug}/document.json"))?;
    let data=serde_json::json!({"url":url,"chatUrl":settings.url("chat/")?,
        "loginUrl":settings.url(&format!("auth/login?return_to={}pages/{slug}/",settings.prefix()))?})
        .to_string().replace('<',"\\u003c");
    Ok(page(
        settings.prefix(),
        &format!("<script id=\"kodrik-document\" type=\"application/json\">{data}</script>"),
    ))
}

fn page(prefix: &str, bootstrap: &str) -> Response {
    let Some(file) = ASSETS.get_file("index.html") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    // Client head configuration is compiled once; these URLs need the runtime prefix.
    let links = format!(
        "<link rel=\"manifest\" href=\"{prefix}chat/manifest.webmanifest\">\
         <link rel=\"apple-touch-icon\" sizes=\"180x180\" href=\"{prefix}chat/apple-touch-icon.png\">\
         <link rel=\"icon\" type=\"image/svg+xml\" href=\"{prefix}chat/icon.svg\">"
    );
    let html = String::from_utf8_lossy(file.contents())
        .replace("/__kodrik_chat__/", prefix)
        .replace("<head>", &format!("<head>{links}{bootstrap}"))
        .replace("<script", &format!("<script nonce=\"{nonce}\""));
    ([
        (header::CONTENT_TYPE, "text/html; charset=utf-8".to_owned()),
        (header::CONTENT_SECURITY_POLICY, format!("default-src 'self'; script-src 'self' 'nonce-{nonce}'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'")),
    ], html).into_response()
}

pub(super) fn response(path: &str) -> Response {
    let Some(file) = ASSETS.get_file(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [(header::CONTENT_TYPE, mime_guess::from_path(path).first_or_octet_stream().to_string()),
         (header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'".into())],
        file.contents(),
    ).into_response()
}
