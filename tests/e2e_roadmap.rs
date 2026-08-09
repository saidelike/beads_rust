//! Public CLI acceptance coverage for the composite roadmap workflow view.

mod common;

use common::cli::{BrWorkspace, run_br, run_br_with_env};
use fsqlite::{Connection, SqliteValue};
use serde_json::Value;
use std::fs;

fn create(workspace: &BrWorkspace, title: &str, issue_type: &str) -> String {
    let result = run_br(
        workspace,
        ["create", title, "--type", issue_type, "--json"],
        &format!("create_{issue_type}"),
    );
    assert!(result.status.success(), "create {title}: {}", result.stderr);
    serde_json::from_str::<Value>(&result.stdout).expect("create JSON")["id"]
        .as_str()
        .expect("created id")
        .to_string()
}

fn add_relation(workspace: &BrWorkspace, issue: &str, target: &str, relation: &str) {
    let result = run_br(
        workspace,
        ["dep", "add", issue, target, "--type", relation],
        &format!("add_{relation}"),
    );
    assert!(
        result.status.success(),
        "add {relation} {issue} -> {target}: {}",
        result.stderr
    );
}

fn enable_matt_profile(workspace: &BrWorkspace) {
    fs::write(
        workspace.root.join(".beads/policy.yaml"),
        "issue_types:\n  profiles: [matt-skills]\n",
    )
    .expect("write Matt profile policy");
}

#[test]
fn roadmap_json_composes_containment_and_traceability_without_execution_edges() {
    let _log = common::test_log(
        "roadmap_json_composes_containment_and_traceability_without_execution_edges",
    );
    let workspace = BrWorkspace::new();
    assert!(run_br(&workspace, ["init"], "init").status.success());

    let map = create(&workspace, "Map", "map");
    let decision = create(&workspace, "Decision", "research");
    let spec = create(&workspace, "Spec", "spec");
    let implementation = create(&workspace, "Implementation", "implementation");

    add_relation(&workspace, &decision, &map, "parent-child");
    add_relation(&workspace, &spec, &map, "derived-from");
    add_relation(&workspace, &implementation, &spec, "implements");
    add_relation(&workspace, &implementation, &spec, "blocks");

    let result = run_br(
        &workspace,
        ["roadmap", &implementation, "--format", "json"],
        "roadmap_json",
    );
    assert!(result.status.success(), "roadmap: {}", result.stderr);
    let output: Value = serde_json::from_str(&result.stdout).expect("roadmap JSON");

    assert_eq!(output["contract_version"], "br.roadmap.v1");
    assert_eq!(output["requested_root"]["id"], implementation);
    assert_eq!(output["roots"][0], map);
    assert_eq!(output["nodes"].as_array().map(Vec::len), Some(4));

    let relations: Vec<&str> = output["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .map(|edge| edge["relation"].as_str().expect("relation"))
        .collect();
    assert_eq!(relations, ["parent-child", "derived-from", "implements"]);
    assert!(!result.stdout.contains("\"relation\": \"blocks\""));
}

#[test]
fn parallel_typed_relations_require_typed_removal_when_ambiguous() {
    let _log = common::test_log("parallel_typed_relations_require_typed_removal_when_ambiguous");
    let workspace = BrWorkspace::new();
    assert!(run_br(&workspace, ["init"], "init").status.success());

    let implementation = create(&workspace, "Implementation", "implementation");
    let spec = create(&workspace, "Spec", "spec");
    add_relation(&workspace, &implementation, &spec, "blocks");
    add_relation(&workspace, &implementation, &spec, "implements");

    let ambiguous = run_br(
        &workspace,
        ["dep", "remove", &implementation, &spec],
        "remove_ambiguous",
    );
    assert!(!ambiguous.status.success());
    assert!(
        ambiguous.stderr.contains("ambiguous"),
        "{}",
        ambiguous.stderr
    );
    assert!(ambiguous.stderr.contains("blocks"), "{}", ambiguous.stderr);
    assert!(
        ambiguous.stderr.contains("implements"),
        "{}",
        ambiguous.stderr
    );

    let typed = run_br(
        &workspace,
        [
            "dep",
            "remove",
            &implementation,
            &spec,
            "--type",
            "implements",
        ],
        "remove_implements",
    );
    assert!(typed.status.success(), "typed removal: {}", typed.stderr);

    let listed = run_br(
        &workspace,
        ["dep", "list", &implementation, "--json"],
        "list_remaining",
    );
    assert!(listed.status.success(), "dep list: {}", listed.stderr);
    assert!(listed.stdout.contains("blocks"));
    assert!(!listed.stdout.contains("implements"));
}

#[test]
fn roadmap_depth_focus_summaries_and_diagrams_share_one_lossless_model() {
    let _log =
        common::test_log("roadmap_depth_focus_summaries_and_diagrams_share_one_lossless_model");
    let workspace = BrWorkspace::new();
    assert!(run_br(&workspace, ["init"], "init").status.success());
    enable_matt_profile(&workspace);

    let map = create(&workspace, "Map", "map");
    let decision = create(&workspace, "Decision", "research");
    let spec = create(&workspace, "Spec", "spec");
    let implementation = create(&workspace, "Implementation", "implementation");
    add_relation(&workspace, &decision, &map, "parent-child");
    add_relation(&workspace, &spec, &map, "derived-from");
    add_relation(&workspace, &implementation, &spec, "implements");

    let result = run_br(
        &workspace,
        [
            "roadmap",
            &implementation,
            "--max-depth",
            "0",
            "--format",
            "json",
        ],
        "roadmap_summary_depth",
    );
    assert!(result.status.success(), "roadmap: {}", result.stderr);
    let output: Value = serde_json::from_str(&result.stdout).expect("roadmap JSON");
    assert_eq!(output["placements"].as_array().map(Vec::len), Some(1));
    assert_eq!(output["placements"][0]["truncated"], true);
    let summaries = output["summaries"].as_array().expect("summaries");
    let map_summary = summaries
        .iter()
        .find(|summary| summary["id"] == map)
        .expect("map summary");
    assert_eq!(map_summary["total"]["count"], 2);
    let spec_summary = summaries
        .iter()
        .find(|summary| summary["id"] == spec)
        .expect("spec summary");
    assert_eq!(spec_summary["total"]["count"], 1);

    let decisions = run_br(
        &workspace,
        ["roadmap", &map, "--focus", "decisions", "--format", "json"],
        "roadmap_decisions",
    );
    assert!(decisions.status.success(), "focus: {}", decisions.stderr);
    let focused: Value = serde_json::from_str(&decisions.stdout).expect("focused JSON");
    let placement_ids: Vec<&str> = focused["placements"]
        .as_array()
        .expect("placements")
        .iter()
        .filter_map(|placement| placement["id"].as_str())
        .collect();
    assert!(placement_ids.contains(&map.as_str()));
    assert!(placement_ids.contains(&decision.as_str()));
    assert!(!placement_ids.contains(&spec.as_str()));
    assert_eq!(focused["edges"].as_array().map(Vec::len), Some(3));
    assert_eq!(
        focused["filters"]["focus"],
        serde_json::json!(["decisions"])
    );
    assert!(focused["filters"].get("only").is_none());

    let both = run_br(
        &workspace,
        [
            "roadmap",
            &map,
            "--focus",
            "decisions,implementations",
            "--format",
            "json",
        ],
        "roadmap_both",
    );
    let both: Value = serde_json::from_str(&both.stdout).expect("both JSON");
    assert_eq!(both["placements"].as_array().map(Vec::len), Some(4));

    let graph_only = run_br(
        &workspace,
        ["roadmap", &map, "--only", "graph", "--format", "json"],
        "roadmap_graph_only",
    );
    assert!(graph_only.status.success(), "{}", graph_only.stderr);
    let graph_only: Value = serde_json::from_str(&graph_only.stdout).expect("graph-only JSON");
    assert_eq!(graph_only["placements"].as_array().map(Vec::len), Some(4));
    assert_eq!(graph_only["summaries"].as_array().map(Vec::len), Some(0));

    for (format, marker) in [("mermaid", "flowchart TD"), ("dot", "digraph roadmap")] {
        let diagram = run_br(
            &workspace,
            ["roadmap", &map, "--format", format],
            &format!("roadmap_{format}"),
        );
        assert!(diagram.status.success(), "{format}: {}", diagram.stderr);
        assert!(diagram.stdout.contains(marker), "{}", diagram.stdout);
        assert!(diagram.stdout.contains("derived-from"));
        assert!(diagram.stdout.contains("implements"));
        assert!(!diagram.stdout.contains("blocks"));
    }
}

#[test]
fn roadmap_rejects_unsupported_or_conflicting_output_combinations() {
    let _log = common::test_log("roadmap_rejects_unsupported_or_conflicting_output_combinations");
    let workspace = BrWorkspace::new();
    assert!(run_br(&workspace, ["init"], "init").status.success());
    let root = create(&workspace, "Root", "epic");

    let toon = run_br_with_env(
        &workspace,
        ["roadmap", &root],
        [("BR_OUTPUT_FORMAT", "toon")],
        "roadmap_toon",
    );
    assert!(!toon.status.success());
    let toon_output = format!("{}\n{}", toon.stdout, toon.stderr);
    assert!(
        toon_output.contains("does not support TOON"),
        "{toon_output}"
    );

    let conflict = run_br(
        &workspace,
        ["roadmap", &root, "--format", "text", "--json"],
        "roadmap_conflict",
    );
    assert!(!conflict.status.success());
    let conflict_output = format!("{}\n{}", conflict.stdout, conflict.stderr);
    assert!(conflict_output.contains("conflicts"), "{conflict_output}");

    let environment_conflict = run_br_with_env(
        &workspace,
        ["roadmap", &root, "--format", "json"],
        [("BR_OUTPUT_FORMAT", "text")],
        "roadmap_environment_conflict",
    );
    assert!(!environment_conflict.status.success());
    let environment_output = format!(
        "{}\n{}",
        environment_conflict.stdout, environment_conflict.stderr
    );
    assert!(
        environment_output.contains("conflicts"),
        "{environment_output}"
    );

    let csv = run_br_with_env(
        &workspace,
        ["roadmap", &root],
        [("BR_OUTPUT_FORMAT", "csv")],
        "roadmap_csv",
    );
    assert!(!csv.status.success());
    let csv_output = format!("{}\n{}", csv.stdout, csv.stderr);
    assert!(csv_output.contains("does not support CSV"), "{csv_output}");

    let diagram_summary = run_br(
        &workspace,
        ["roadmap", &root, "--format", "dot", "--only", "summary"],
        "roadmap_dot_summary",
    );
    assert!(!diagram_summary.status.success());
    assert!(diagram_summary.stderr.contains("summary"));

    for (label, args) in [
        ("legacy_summary", vec!["roadmap", &root, "--summary"]),
        (
            "legacy_summary_only",
            vec!["roadmap", &root, "--summary-only"],
        ),
        (
            "legacy_role_only",
            vec!["roadmap", &root, "--only", "decisions"],
        ),
    ] {
        let legacy = run_br(&workspace, args, label);
        assert!(!legacy.status.success(), "{label}: {}", legacy.stdout);
    }
}

#[test]
fn roadmap_quiet_mode_still_validates_missing_roots() {
    let _log = common::test_log("roadmap_quiet_mode_still_validates_missing_roots");
    let workspace = BrWorkspace::new();
    assert!(run_br(&workspace, ["init"], "init").status.success());

    let missing = run_br(
        &workspace,
        ["--quiet", "roadmap", "definitely-not-a-roadmap-root"],
        "roadmap_quiet_missing",
    );

    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty(), "{}", missing.stdout);
}

#[test]
fn roadmap_stops_upstream_at_authorities_and_applies_focus_to_the_requested_node() {
    let _log = common::test_log(
        "roadmap_stops_upstream_at_authorities_and_applies_focus_to_the_requested_node",
    );
    let workspace = BrWorkspace::new();
    assert!(run_br(&workspace, ["init"], "init").status.success());
    enable_matt_profile(&workspace);

    let epic = create(&workspace, "Unclassified container", "epic");
    let map = create(&workspace, "Roadmap authority", "map");
    let decision = create(&workspace, "Planning decision", "research");
    let spec = create(&workspace, "Published specification", "spec");
    let implementation = create(&workspace, "Delivery slice", "implementation");
    add_relation(&workspace, &map, &epic, "parent-child");
    add_relation(&workspace, &decision, &map, "parent-child");
    add_relation(&workspace, &spec, &map, "derived-from");
    add_relation(&workspace, &implementation, &spec, "implements");

    let rerooted = run_br(
        &workspace,
        ["roadmap", &spec, "--format", "json"],
        "roadmap_authority_boundary",
    );
    assert!(rerooted.status.success(), "{}", rerooted.stderr);
    let rerooted: Value = serde_json::from_str(&rerooted.stdout).expect("rerooted JSON");
    assert_eq!(rerooted["roots"], serde_json::json!([map]));

    for (requested, focus) in [(&spec, "decisions"), (&decision, "implementations")] {
        let focused = run_br(
            &workspace,
            ["roadmap", requested, "--focus", focus, "--format", "json"],
            &format!("roadmap_requested_{focus}"),
        );
        assert!(focused.status.success(), "{focus}: {}", focused.stderr);
        let focused: Value = serde_json::from_str(&focused.stdout).expect("focused JSON");
        assert!(
            focused["placements"]
                .as_array()
                .expect("placements")
                .iter()
                .all(|placement| placement["id"] != requested.as_str()),
            "{focus}: {focused}"
        );
        assert_eq!(focused["requested_root"]["marked"], false);
    }
}

#[test]
fn traceability_relations_do_not_change_scheduler_dependency_impact() {
    let _log = common::test_log("traceability_relations_do_not_change_scheduler_dependency_impact");
    let workspace = BrWorkspace::new();
    assert!(run_br(&workspace, ["init"], "init").status.success());
    let first = create(&workspace, "First", "task");
    let second = create(&workspace, "Second", "task");

    let before = run_br(
        &workspace,
        ["scheduler", "--json", "--limit", "10"],
        "scheduler_before",
    );
    assert!(before.status.success(), "scheduler: {}", before.stderr);
    let before: Value = serde_json::from_str(&before.stdout).expect("scheduler before JSON");
    add_relation(&workspace, &first, &second, "implements");
    let after = run_br(
        &workspace,
        ["scheduler", "--json", "--limit", "10"],
        "scheduler_after",
    );
    assert!(after.status.success(), "scheduler: {}", after.stderr);
    let after: Value = serde_json::from_str(&after.stdout).expect("scheduler after JSON");

    for id in [&first, &second] {
        let before_row = before["recommendations"]
            .as_array()
            .expect("before recommendations")
            .iter()
            .find(|row| row["issue"]["id"] == *id)
            .expect("before row");
        let after_row = after["recommendations"]
            .as_array()
            .expect("after recommendations")
            .iter()
            .find(|row| row["issue"]["id"] == *id)
            .expect("after row");
        assert_eq!(
            before_row["evidence"]["dependency_impact"],
            after_row["evidence"]["dependency_impact"]
        );
        assert_eq!(before_row["score"], after_row["score"]);
    }
}

#[test]
fn roadmap_reroots_to_a_deterministic_forest_and_marks_hidden_requested_routes() {
    let _log = common::test_log(
        "roadmap_reroots_to_a_deterministic_forest_and_marks_hidden_requested_routes",
    );
    let workspace = BrWorkspace::new();
    assert!(run_br(&workspace, ["init"], "init").status.success());
    enable_matt_profile(&workspace);

    let first_map = create(&workspace, "Alpha authority", "map");
    let second_map = create(&workspace, "Beta authority", "map");
    let spec = create(&workspace, "Shared specification", "spec");
    let implementation = create(&workspace, "Requested implementation", "implementation");
    add_relation(&workspace, &spec, &first_map, "derived-from");
    add_relation(&workspace, &spec, &second_map, "derived-from");
    add_relation(&workspace, &implementation, &spec, "implements");

    let truncated = run_br(
        &workspace,
        [
            "roadmap",
            &implementation,
            "--max-depth",
            "0",
            "--format",
            "json",
        ],
        "roadmap_truncated_forest",
    );
    assert!(truncated.status.success(), "{}", truncated.stderr);
    let truncated: Value = serde_json::from_str(&truncated.stdout).expect("truncated JSON");
    assert_eq!(
        truncated["roots"],
        serde_json::json!([first_map, second_map])
    );
    assert_eq!(truncated["requested_root"]["marked"], false);
    assert_eq!(truncated["truncation"]["requested_hidden"], true);
    assert!(
        truncated["placements"]
            .as_array()
            .expect("placements")
            .iter()
            .all(|placement| placement["requested_route"] == true)
    );

    let complete = run_br(
        &workspace,
        ["roadmap", &implementation, "--format", "json"],
        "roadmap_complete_forest",
    );
    assert!(complete.status.success(), "{}", complete.stderr);
    let complete: Value = serde_json::from_str(&complete.stdout).expect("complete JSON");
    assert!(
        complete["requested_root"]["marked"]
            .as_bool()
            .unwrap_or(false)
    );
    assert_eq!(
        complete["placements"]
            .as_array()
            .expect("placements")
            .iter()
            .filter(|placement| placement["reference"] == true)
            .count(),
        1
    );
    assert!(
        complete["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "shared_reference")
    );
}

#[test]
fn roadmap_cycle_and_unlinked_role_are_recoverable_diagnostics() {
    let _log = common::test_log("roadmap_cycle_and_unlinked_role_are_recoverable_diagnostics");
    let workspace = BrWorkspace::new();
    assert!(run_br(&workspace, ["init"], "init").status.success());
    enable_matt_profile(&workspace);

    let unlinked = create(&workspace, "Unlinked implementation", "implementation");
    let local = run_br(
        &workspace,
        ["roadmap", &unlinked, "--format", "json"],
        "roadmap_unlinked",
    );
    assert!(local.status.success(), "{}", local.stderr);
    let local: Value = serde_json::from_str(&local.stdout).expect("unlinked JSON");
    assert_eq!(local["placements"][0]["id"], unlinked);
    assert!(
        local["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "unlinked_role_root")
    );

    let map = create(&workspace, "Cycle map", "map");
    let spec = create(&workspace, "Cycle spec", "spec");
    let implementation = create(&workspace, "Cycle implementation", "implementation");
    add_relation(&workspace, &spec, &map, "derived-from");
    add_relation(&workspace, &implementation, &spec, "implements");
    add_relation(&workspace, &map, &implementation, "derived-from");
    let cycle = run_br(
        &workspace,
        ["roadmap", &implementation, "--format", "json"],
        "roadmap_cycle",
    );
    assert!(
        cycle.status.success(),
        "recoverable cycle: {}",
        cycle.stderr
    );
    let cycle: Value = serde_json::from_str(&cycle.stdout).expect("cycle JSON");
    assert!(
        cycle["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|diagnostic| matches!(diagnostic["code"].as_str(), Some("cycle" | "cycle_root")))
    );
}

#[test]
fn roadmap_preserves_missing_endpoints_in_json_and_diagrams() {
    let _log = common::test_log("roadmap_preserves_missing_endpoints_in_json_and_diagrams");
    let workspace = BrWorkspace::new();
    assert!(run_br(&workspace, ["init"], "init").status.success());
    enable_matt_profile(&workspace);
    let implementation = create(
        &workspace,
        "Implementation with missing contract",
        "implementation",
    );

    let connection = Connection::open(
        workspace
            .root
            .join(".beads/beads.db")
            .to_string_lossy()
            .into_owned(),
    )
    .expect("open raw malformed fixture database");
    connection
        .execute_with_params(
            "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at) \
             VALUES (?, 'bd-missing-spec', 'implements', '2026-01-01T00:00:00Z')",
            &[SqliteValue::from(implementation.as_str())],
        )
        .expect("insert missing roadmap endpoint");
    connection
        .close()
        .expect("close malformed fixture database");

    let json = run_br(
        &workspace,
        ["roadmap", &implementation, "--format", "json"],
        "roadmap_missing_json",
    );
    assert!(json.status.success(), "{}", json.stderr);
    let json: Value = serde_json::from_str(&json.stdout).expect("missing endpoint JSON");
    assert!(
        json["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .any(|node| node["id"] == "bd-missing-spec" && node["missing"] == true)
    );
    assert!(
        json["placements"]
            .as_array()
            .expect("placements")
            .iter()
            .any(|placement| placement["id"] == implementation)
    );
    assert!(
        json["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "missing_endpoint")
    );

    for format in ["mermaid", "dot"] {
        let diagram = run_br(
            &workspace,
            ["roadmap", &implementation, "--format", format],
            &format!("roadmap_missing_{format}"),
        );
        assert!(diagram.status.success(), "{format}: {}", diagram.stderr);
        assert!(diagram.stdout.contains("bd-missing-spec"));
        assert!(diagram.stdout.contains("implements"));
        assert!(diagram.stderr.contains("missing_endpoint"));
    }
}

#[test]
fn roadmap_summary_only_omits_tree_payload_and_preserves_direct_closed_counts() {
    let _log = common::test_log(
        "roadmap_summary_only_omits_tree_payload_and_preserves_direct_closed_counts",
    );
    let workspace = BrWorkspace::new();
    assert!(run_br(&workspace, ["init"], "init").status.success());
    enable_matt_profile(&workspace);
    let map = create(&workspace, "Summary map", "map");
    let decision = create(&workspace, "Closed decision", "research");
    let spec = create(&workspace, "Open specification", "spec");
    let template = create(&workspace, "Decision template", "research");
    add_relation(&workspace, &decision, &map, "parent-child");
    add_relation(&workspace, &spec, &map, "derived-from");
    add_relation(&workspace, &template, &map, "parent-child");
    let connection = Connection::open(
        workspace
            .root
            .join(".beads/beads.db")
            .to_string_lossy()
            .into_owned(),
    )
    .expect("open summary fixture database");
    connection
        .execute_with_params(
            "UPDATE issues SET is_template = 1 WHERE id = ?",
            &[SqliteValue::from(template.as_str())],
        )
        .expect("mark roadmap template");
    connection.close().expect("close summary fixture database");
    let closed = run_br(
        &workspace,
        ["close", &decision, "--reason", "Decision complete"],
        "close_decision",
    );
    assert!(closed.status.success(), "{}", closed.stderr);

    let summary = run_br(
        &workspace,
        ["roadmap", &map, "--only", "summary", "--format", "json"],
        "roadmap_summary_only",
    );
    assert!(summary.status.success(), "{}", summary.stderr);
    let summary: Value = serde_json::from_str(&summary.stdout).expect("summary JSON");
    for field in ["roots", "nodes", "edges", "placements"] {
        assert_eq!(summary[field].as_array().map(Vec::len), Some(0), "{field}");
    }
    let map_summary = summary["summaries"]
        .as_array()
        .expect("summaries")
        .iter()
        .find(|scope| scope["id"] == map)
        .expect("map summary");
    assert_eq!(map_summary["scope"], "directly related issues");
    assert_eq!(map_summary["total"]["count"], 2);
    assert_eq!(map_summary["total"]["terminal"], 1);
    assert_eq!(map_summary["total"]["completion_percent"], 50);
    let spec_summary = summary["summaries"]
        .as_array()
        .expect("summaries")
        .iter()
        .find(|scope| scope["id"] == spec)
        .expect("spec summary");
    assert_eq!(spec_summary["total"]["count"], 0);
    assert!(spec_summary["total"]["completion_percent"].is_null());
}

#[test]
fn roadmap_schema_and_established_dependency_views_remain_discoverable() {
    let _log =
        common::test_log("roadmap_schema_and_established_dependency_views_remain_discoverable");
    let workspace = BrWorkspace::new();
    assert!(run_br(&workspace, ["init"], "init").status.success());
    enable_matt_profile(&workspace);
    let map = create(&workspace, "Compatibility map", "map");
    let decision = create(&workspace, "Compatibility decision", "research");
    let spec = create(&workspace, "Compatibility spec", "spec");
    let implementation = create(&workspace, "Compatibility implementation", "implementation");
    add_relation(&workspace, &decision, &map, "parent-child");
    add_relation(&workspace, &spec, &map, "derived-from");
    add_relation(&workspace, &implementation, &spec, "implements");

    let capabilities = run_br(
        &workspace,
        ["capabilities", "--format", "json"],
        "roadmap_capabilities",
    );
    assert!(capabilities.status.success(), "{}", capabilities.stderr);
    let capabilities: Value =
        serde_json::from_str(&capabilities.stdout).expect("capabilities JSON");
    assert_eq!(
        capabilities["issue_types"]["types"]["map"]["roadmap_role"],
        "authority"
    );
    assert_eq!(
        capabilities["issue_types"]["types"]["spec"]["roadmap_role"],
        "specification"
    );

    let schema = run_br(
        &workspace,
        ["schema", "roadmap", "--format", "json"],
        "roadmap_schema",
    );
    assert!(schema.status.success(), "{}", schema.stderr);
    assert!(schema.stdout.contains("Roadmap"), "{}", schema.stdout);
    assert!(
        schema.stdout.contains("contract_version"),
        "{}",
        schema.stdout
    );

    let help = run_br(&workspace, ["--help"], "top_level_help");
    assert!(help.status.success(), "{}", help.stderr);
    assert!(!help.stdout.contains("\n  tree "), "{}", help.stdout);
    assert!(!help.stdout.contains("\n  tree-status "), "{}", help.stdout);

    for (label, args) in [
        ("tree", vec!["tree", map.as_str()]),
        ("tree_status", vec!["tree-status", map.as_str()]),
    ] {
        let result = run_br(&workspace, args, label);
        assert!(!result.status.success(), "{label}: {}", result.stdout);
    }

    for (label, args) in [
        (
            "dep_tree",
            vec!["dep", "tree", implementation.as_str(), "--json"],
        ),
        ("graph", vec!["graph", implementation.as_str(), "--json"]),
    ] {
        let result = run_br(&workspace, args, label);
        assert!(result.status.success(), "{label}: {}", result.stderr);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn roadmap_rich_and_forced_plain_modes_follow_terminal_contracts() {
    let _log = common::test_log("roadmap_rich_and_forced_plain_modes_follow_terminal_contracts");
    let workspace = BrWorkspace::new();
    assert!(run_br(&workspace, ["init"], "init").status.success());
    enable_matt_profile(&workspace);
    let map = create(
        &workspace,
        "A roadmap authority with a deliberately long title for terminal wrapping",
        "map",
    );
    let spec = create(&workspace, "A specification child", "spec");
    add_relation(&workspace, &spec, &map, "derived-from");
    let closed = run_br(
        &workspace,
        ["close", &spec, "--reason", "Specification complete"],
        "close_rich_spec",
    );
    assert!(closed.status.success(), "{}", closed.stderr);

    let br = assert_cmd::cargo::cargo_bin!("br");
    for (format_args, expect_ansi) in [("", true), ("--format text", false)] {
        let command = format!(
            "stty cols 64 rows 40 && {} roadmap {} --wrap --no-auto-import {format_args}",
            br.display(),
            map
        );
        let output = std::process::Command::new("script")
            .args(["-qec", &command, "/dev/null"])
            .current_dir(&workspace.root)
            .env("HOME", &workspace.root)
            .env("RUST_LOG", "error")
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("COLUMNS", "64")
            .env_remove("NO_COLOR")
            .env_remove("BR_OUTPUT_FORMAT")
            .env_remove("TOON_DEFAULT_FORMAT")
            .env_remove("BEADS_DIR")
            .env_remove("BEADS_JSONL")
            .output()
            .expect("run roadmap under pseudo-TTY");
        assert!(output.status.success(), "script status: {}", output.status);
        let stdout = String::from_utf8_lossy(&output.stdout).replace('\r', "");
        assert_eq!(stdout.contains("\u{1b}["), expect_ansi, "{stdout}");
        assert!(stdout.contains("Roadmap Graph"), "{stdout}");
        if expect_ansi {
            assert!(stdout.contains("╰──"), "rounded roadmap guide: {stdout}");
            assert!(stdout.contains('╭'), "outlined summary scope: {stdout}");
            assert!(stdout.contains('━'), "summary progress bar: {stdout}");
        }
    }
}
