use agent_client_protocol::v2::schema;
use serde_json::json;

/// ACP implementation information for the agent or an Elphie client.
pub fn implementation(name: &str, title: &str, version: &str) -> schema::Implementation {
    let icon = schema::Icon::new(elphie_core::icon_data_uri())
        .mime_type("image/webp")
        .sizes(vec!["192x192".into()]);
    let mut meta = schema::Meta::new();
    meta.insert("io.github.thiskevinwang.elphie/icon".into(), json!(icon));
    schema::Implementation::new(name, version)
        .title(title)
        .meta(meta)
}
