//! E2E coverage for inherited governing context on `br show`
//! (beads_rust#297, beads_rust#351).
//!
//! beads_rust#351 regression: when several siblings beneath the same
//! epic are shown in one invocation, the shared ancestor block used to
//! be re-rendered once per sibling because each child's ancestor chain
//! resolves independently. The contract is now: each inherited source
//! is emitted exactly once per invocation, before the first child that
//! references it.

mod common;

use common::cli::{BrWorkspace, parse_created_id, run_br, run_br_with_env};
use std::fs;

fn init_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    workspace
}

/// Build an epic carrying `agent_context` plus two children parented
/// beneath it. Returns `(epic_id, child_a, child_b)`.
fn epic_with_two_children(workspace: &BrWorkspace) -> (String, String, String) {
    let epic = run_br(
        workspace,
        ["create", "Auth rewrite epic", "--type", "epic"],
        "create_epic",
    );
    assert!(epic.status.success(), "create epic failed: {}", epic.stderr);
    let epic_id = parse_created_id(&epic.stdout);
    assert!(!epic_id.is_empty(), "missing epic id: {}", epic.stdout);

    let set_ctx = run_br(
        workspace,
        [
            "update",
            &epic_id,
            "--agent-context",
            r#"{"skills":["clean-code"],"constraints":["no-breaking-changes"]}"#,
        ],
        "set_agent_context",
    );
    assert!(
        set_ctx.status.success(),
        "set agent context failed: {}",
        set_ctx.stderr
    );

    let mut child_ids = Vec::new();
    for (title, label) in [
        ("Token refresh child", "create_child_a"),
        ("Session storage child", "create_child_b"),
    ] {
        let child = run_br(workspace, ["create", title, "--parent", &epic_id], label);
        assert!(
            child.status.success(),
            "create child failed: {}",
            child.stderr
        );
        let child_id = parse_created_id(&child.stdout);
        assert!(!child_id.is_empty(), "missing child id: {}", child.stdout);
        child_ids.push(child_id);
    }

    let child_b = child_ids.pop().expect("child b id");
    let child_a = child_ids.pop().expect("child a id");
    (epic_id, child_a, child_b)
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

#[test]
fn e2e_show_two_siblings_emits_shared_inherited_context_once() {
    let _log = common::test_log("e2e_show_two_siblings_emits_shared_inherited_context_once");
    let workspace = init_workspace();
    let (epic_id, child_a, child_b) = epic_with_two_children(&workspace);

    let show = run_br_with_env(
        &workspace,
        ["show", &child_a, &child_b],
        [("BR_INHERITED_CONTEXT", "1")],
        "show_two_siblings",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let header_marker = format!("--- Inherited context (from epic {epic_id}");
    assert_eq!(
        count_occurrences(&show.stdout, &header_marker),
        1,
        "shared inherited block must be emitted exactly once for sibling \
         children (beads_rust#351), got output:\n{}",
        show.stdout
    );
    assert_eq!(
        count_occurrences(&show.stdout, "clean-code"),
        1,
        "inherited content body must not repeat per sibling:\n{}",
        show.stdout
    );

    // The block must precede the first child referencing it, and both
    // children must still render their own details.
    let block_pos = show
        .stdout
        .find(&header_marker)
        .expect("inherited block present");
    let child_a_pos = show
        .stdout
        .find(&child_a)
        .expect("first child id present in output");
    assert!(
        block_pos < child_a_pos,
        "inherited block should precede the first child referencing it:\n{}",
        show.stdout
    );
    assert!(
        show.stdout.contains("Token refresh child"),
        "first sibling missing:\n{}",
        show.stdout
    );
    assert!(
        show.stdout.contains("Session storage child"),
        "second sibling missing:\n{}",
        show.stdout
    );
}

#[test]
fn e2e_show_single_child_still_emits_inherited_context() {
    let _log = common::test_log("e2e_show_single_child_still_emits_inherited_context");
    let workspace = init_workspace();
    let (epic_id, child_a, _child_b) = epic_with_two_children(&workspace);

    let show = run_br_with_env(
        &workspace,
        ["show", &child_a],
        [("BR_INHERITED_CONTEXT", "1")],
        "show_single_child",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let header_marker = format!("--- Inherited context (from epic {epic_id}");
    assert_eq!(
        count_occurrences(&show.stdout, &header_marker),
        1,
        "single-child show keeps exactly one inherited block:\n{}",
        show.stdout
    );
}

#[test]
fn e2e_show_without_opt_in_emits_no_inherited_context() {
    let _log = common::test_log("e2e_show_without_opt_in_emits_no_inherited_context");
    let workspace = init_workspace();
    let (_epic_id, child_a, child_b) = epic_with_two_children(&workspace);

    let show = run_br(&workspace, ["show", &child_a, &child_b], "show_no_opt_in");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    assert!(
        !show.stdout.contains("Inherited context"),
        "inherited context emission is opt-in (beads_rust#297):\n{}",
        show.stdout
    );
}

#[test]
fn e2e_claim_inherits_context_from_closed_registered_map() {
    let _log = common::test_log("e2e_claim_inherits_context_from_closed_registered_map");
    let workspace = init_workspace();
    fs::write(
        workspace.root.join(".beads").join("policy.yaml"),
        "issue_types:\n  types:\n    - name: map\n      capabilities:\n        aggregate: true\n        blocked_by_open_children: false\n        inherited_context_root: true\n    - name: spec\n      capabilities:\n        aggregate: true\n        may_close_with_open_children: true\n",
    )
    .expect("write issue-type policy");

    let map = run_br(
        &workspace,
        ["create", "Strategic map", "--type", "map"],
        "create_map",
    );
    assert!(map.status.success(), "create map failed: {}", map.stderr);
    let map_id = parse_created_id(&map.stdout);
    let context = run_br(
        &workspace,
        [
            "update",
            &map_id,
            "--agent-context",
            r#"{"strategy":"preserve the published interface"}"#,
        ],
        "set_map_context",
    );
    assert!(
        context.status.success(),
        "set context failed: {}",
        context.stderr
    );

    let spec = run_br(
        &workspace,
        [
            "create",
            "Published spec",
            "--type",
            "spec",
            "--parent",
            &map_id,
        ],
        "create_spec",
    );
    assert!(spec.status.success(), "create spec failed: {}", spec.stderr);
    let spec_id = parse_created_id(&spec.stdout);
    let implementation = run_br(
        &workspace,
        [
            "create",
            "Implementation slice",
            "--type",
            "implementation",
            "--parent",
            &spec_id,
        ],
        "create_implementation",
    );
    assert!(
        implementation.status.success(),
        "create implementation failed: {}",
        implementation.stderr
    );
    let implementation_id = parse_created_id(&implementation.stdout);

    let publish = run_br(&workspace, ["close", &spec_id], "publish_spec");
    assert!(
        publish.status.success(),
        "publish spec failed: {}",
        publish.stderr
    );
    let close_map = run_br(&workspace, ["close", &map_id], "close_map");
    assert!(
        close_map.status.success(),
        "close map failed: {}",
        close_map.stderr
    );

    let claim = run_br_with_env(
        &workspace,
        ["update", &implementation_id, "--claim"],
        [("BR_INHERITED_CONTEXT", "1")],
        "claim_implementation",
    );
    assert!(claim.status.success(), "claim failed: {}", claim.stderr);
    assert!(
        claim.stdout.contains(&format!("root ancestor {map_id}")),
        "closed Map must remain the strategic root:\n{}",
        claim.stdout
    );
    assert!(
        claim.stdout.contains("preserve the published interface"),
        "Map context missing from claim output:\n{}",
        claim.stdout
    );
}

#[test]
fn e2e_show_prefers_nearest_nested_registered_context_root() {
    let _log = common::test_log("e2e_show_prefers_nearest_nested_registered_context_root");
    let workspace = init_workspace();
    fs::write(
        workspace.root.join(".beads").join("policy.yaml"),
        "issue_types:\n  types:\n    - name: map\n      capabilities:\n        aggregate: true\n        inherited_context_root: true\n",
    )
    .expect("write issue-type policy");

    let create_map = |title: &str, parent: Option<&str>, label: &str| {
        let mut args = vec!["create", title, "--type", "map"];
        if let Some(parent) = parent {
            args.extend(["--parent", parent]);
        }
        let output = run_br(&workspace, args, label);
        assert!(
            output.status.success(),
            "create map failed: {}",
            output.stderr
        );
        parse_created_id(&output.stdout)
    };
    let outer_id = create_map("Outer map", None, "create_outer_map");
    let inner_id = create_map("Inner map", Some(&outer_id), "create_inner_map");
    for (id, context, label) in [
        (
            &outer_id,
            r#"{"context":"outer strategy"}"#,
            "outer_context",
        ),
        (
            &inner_id,
            r#"{"context":"inner strategy"}"#,
            "inner_context",
        ),
    ] {
        let update = run_br(
            &workspace,
            ["update", id, "--agent-context", context],
            label,
        );
        assert!(
            update.status.success(),
            "set context failed: {}",
            update.stderr
        );
    }
    let leaf = run_br(
        &workspace,
        ["create", "Nested decision", "--parent", &inner_id],
        "create_nested_leaf",
    );
    assert!(leaf.status.success(), "create leaf failed: {}", leaf.stderr);
    let leaf_id = parse_created_id(&leaf.stdout);

    let show = run_br_with_env(
        &workspace,
        ["show", &leaf_id],
        [("BR_INHERITED_CONTEXT", "1")],
        "show_nested_leaf",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    assert!(show.stdout.contains("inner strategy"), "{}", show.stdout);
    assert!(!show.stdout.contains("outer strategy"), "{}", show.stdout);
    assert!(
        show.stdout.contains(&format!("root ancestor {inner_id}")),
        "{}",
        show.stdout
    );
}
