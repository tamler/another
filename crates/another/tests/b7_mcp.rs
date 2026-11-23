use another::mcp::AnotherMcp;
use another_core::{Clock, Store};
use rmcp::{
    model::{CallToolRequestParams, ClientConfig},
    ServiceExt,
};

#[tokio::test]
async fn default_mcp_has_two_tools_and_no_name_only_mutation() {
    let store = Store::open_in_memory(Clock::System).unwrap();
    let before = store.events(500).unwrap().len();
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(AnotherMcp::new(store.clone()).serve(server_io));
    let client = ClientConfig::default().serve(client_io).await.unwrap();
    let server = server.await.unwrap().unwrap();
    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(names, vec!["another_identity", "another_lease"]);
    for name in ["hold_paths", "send_message", "queue_next", "another_lease"] {
        let result = client
            .call_tool(CallToolRequestParams::new(name))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(serde_json::to_string(&result).unwrap().contains("NO_ACTOR"));
    }
    assert_eq!(store.events(500).unwrap().len(), before);
    client.cancel().await.unwrap();
    server.cancel().await.unwrap();
}
