use std::path::{Path, PathBuf};

use serde_json::{json, Value};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(root().join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

fn json(relative: &str) -> Value {
    serde_json::from_str(&read(relative))
        .unwrap_or_else(|error| panic!("parse {relative}: {error}"))
}

#[test]
fn claude_marketplace_and_mcp_assets_are_installable() {
    let marketplace = json(".claude-plugin/marketplace.json");
    assert_eq!(marketplace["name"], "copilot-api-rust-marketplace");
    assert_eq!(marketplace["plugins"].as_array().unwrap().len(), 2);
    for plugin in marketplace["plugins"].as_array().unwrap() {
        let source = plugin["source"].as_str().unwrap().trim_start_matches("./");
        assert!(root()
            .join(source)
            .join(".claude-plugin/plugin.json")
            .is_file());
    }

    let mcp = json("plugin/claude/tool-search/.mcp.json");
    assert_eq!(mcp["mcpServers"]["tool_search"]["command"], "copilot-api");
    assert_eq!(mcp["mcpServers"]["tool_search"]["args"], json!(["mcp"]));
    assert_eq!(mcp["mcpServers"]["tool_search"]["alwaysLoad"], true);
}

#[test]
fn marker_assets_match_the_gateway_protocol() {
    let claude = read("plugin/claude/agent-inject/scripts/subagent-start.js");
    let opencode = read("plugin/opencode/subagent-marker.js");
    for asset in [claude, opencode] {
        assert!(asset.contains("__SUBAGENT_MARKER__"));
        assert!(asset.contains("session_id"));
        assert!(asset.contains("agent_id"));
        assert!(asset.contains("agent_type"));
    }

    let payload = json!({
        "messages": [{
            "role": "user",
            "content": [{
                "type": "text",
                "text": "<system-reminder>\nSubagentStart hook additional context: __SUBAGENT_MARKER__{\"session_id\":\"session\",\"agent_id\":\"agent\",\"agent_type\":\"worker\"}\n</system-reminder>"
            }]
        }]
    });
    let marker =
        copilot_api::libs::subagent::parse_subagent_marker_from_first_user(&payload).unwrap();
    assert_eq!(marker.session_id, "session");
    assert_eq!(marker.agent_id, "agent");
    assert_eq!(marker.agent_type, "worker");
}

#[test]
fn integration_assets_contain_no_machine_paths_or_credentials() {
    let assets = [
        read(".claude-plugin/marketplace.json"),
        read("plugin/claude/agent-inject/hooks/hooks.json"),
        read("plugin/claude/agent-inject/scripts/session-start.js"),
        read("plugin/claude/agent-inject/scripts/subagent-start.js"),
        read("plugin/claude/tool-search/.mcp.json"),
        read("plugin/opencode/subagent-marker.js"),
        read("plugin/opencode/opencode.example.json"),
    ]
    .join("\n");
    for forbidden in [
        "/Users/",
        "/home/",
        "C:\\Users\\",
        "sk-",
        "ghp_",
        "Bearer ",
        "provider-secret",
    ] {
        assert!(
            !assets.contains(forbidden),
            "integration asset contains forbidden pattern {forbidden}"
        );
    }
}

#[test]
fn integration_version_evidence_is_pinned() {
    let versions = json("plugin/versions.json");
    assert_eq!(versions["reference"]["commit"].as_str().unwrap().len(), 40);
    for client in ["claudeCode", "codexCli", "openCode"] {
        assert!(!versions["validatedClients"][client]
            .as_str()
            .unwrap()
            .is_empty());
    }
}

#[test]
fn tool_search_plugin_matches_deferred_tool_selection() {
    let tools = vec![
        json!({"name":"mcp__tool_search__search","description":"bridge","input_schema":{"type":"object"}}),
        json!({
            "name":"large_deferred_tool",
            "description":"loaded only when selected",
            "input_schema":{"type":"object"},
            "defer_loading":true
        }),
    ];
    assert!(
        copilot_api::libs::tool_search::should_enable_responses_tool_search(
            "gpt-5.4",
            Some(&tools)
        )
    );
    assert!(
        !copilot_api::libs::tool_search::should_enable_responses_tool_search(
            "claude-opus-4.8",
            Some(&tools)
        )
    );
}
