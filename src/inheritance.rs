//! Inherited agent context (beads_rust#297).
//!
//! When a descendant bead is shown or transitioned into `in_progress`
//! (via `br update --status in_progress` or `--claim`), the bead's
//! ancestors' `agent_context` field is emitted alongside the normal
//! output. This re-delivers governing instructions at the surface
//! where the agent actually interacts with `br`, mitigating three
//! failure modes:
//!
//! 1. **Cold-start miss** — agent claims a child and never reads the
//!    epic.
//! 2. **Context decay** — agent had the instructions at session start
//!    but compaction has dropped them.
//! 3. **Stale propagation** — ancestor instructions were updated mid-
//!    flight but descendants read only the cold-start snapshot.
//!
//! Emission is **opt-in** per project (off by default for backward
//! compatibility): set `inherited_context.enabled: true` in
//! `.beads/config.yaml`, or set `BR_INHERITED_CONTEXT=1` in the
//! environment. The env var wins so operators can toggle behavior
//! without committing config changes.
//!
//! v1 emits the **two bookends** of the ancestor chain — the immediate
//! parent and the strategic root ancestor. Projects with configured type
//! capabilities prefer the nearest ancestor marked `inherited_context_root`;
//! projects without type policy preserve the topmost-Epic behavior. Both
//! modes otherwise fall back to the chain's terminal ancestor. Intermediate
//! layers are skipped; if parent == root, only one block is emitted. Cycles
//! in the parent chain stop traversal and log to stderr; tombstoned and
//! missing ancestors are silently skipped.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::{Issue, IssueType};
use crate::storage::sqlite::SqliteStorage;

/// One ancestor block in the inherited-context emission.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InheritedBlock {
    /// The ancestor's bead id (e.g. "bd-abc123").
    pub source_id: String,
    /// "epic" when the ancestor is the root and has `issue_type = epic`,
    /// "root" when the ancestor is the root but not an epic,
    /// "parent" when the ancestor is the immediate parent.
    pub source_role: String,
    /// The ancestor's title for human-readable headings.
    pub source_title: String,
    /// Which field on the ancestor supplied the content (e.g.
    /// "agent_context"). Per-config-field-list resolution: in a future
    /// extension callers can configure `[agent_context, design]` and
    /// the first present wins.
    pub field_used: String,
    /// The raw content, as stored. For `agent_context` this is the
    /// canonical JSON string; downstream renderers can parse it for
    /// pretty-printing.
    pub content: String,
}

/// Returns true when the project has opted in to inherited-context
/// emission. The env var `BR_INHERITED_CONTEXT` (any non-empty,
/// non-"0", non-"false" value) wins; otherwise checks
/// `.beads/config.yaml` for `inherited_context.enabled: true`.
#[must_use]
pub fn is_enabled(beads_dir: &Path) -> bool {
    let env_val = std::env::var("BR_INHERITED_CONTEXT").ok();
    is_enabled_from_inputs(env_val.as_deref(), &beads_dir.join("config.yaml"))
}

/// Pure helper for `is_enabled`: given the env var value (or `None`)
/// and a path to `config.yaml`, decide whether emission is on. Lets
/// tests cover the truthiness rules without touching the process
/// environment (which would require `unsafe { set_var }` under edition
/// 2024 / `#![forbid(unsafe_code)]`).
#[must_use]
pub fn is_enabled_from_inputs(env_val: Option<&str>, config_yaml_path: &Path) -> bool {
    if let Some(env_val) = env_val {
        let trimmed = env_val.trim();
        return !trimmed.is_empty()
            && !trimmed.eq_ignore_ascii_case("0")
            && !trimmed.eq_ignore_ascii_case("false")
            && !trimmed.eq_ignore_ascii_case("no");
    }

    let Ok(contents) = std::fs::read_to_string(config_yaml_path) else {
        return false;
    };
    // Cheap-and-correct check: parse as generic YAML and look up the
    // `inherited_context.enabled` path. Avoids strong-typing the
    // config struct because the project's main config.yaml schema is
    // open-shaped and the inherited_context section may not exist on
    // older projects.
    let Ok(value): std::result::Result<serde_yml::Value, _> = serde_yml::from_str(&contents) else {
        return false;
    };
    value
        .get("inherited_context")
        .and_then(|node| node.get("enabled"))
        .and_then(serde_yml::Value::as_bool)
        .unwrap_or(false)
}

/// Walk the parent chain of `child_id` and collect at most two
/// inherited blocks: the immediate parent (if it has `agent_context`)
/// and the root ancestor (if it has `agent_context`). If parent ==
/// root, returns a single block. Ancestors with no `agent_context` are
/// silently skipped. Cycles stop traversal and log to stderr.
///
/// Returns an empty vec when the bead has no parent or when none of
/// the bookends supply content.
///
/// # Errors
///
/// Returns an error only on a storage failure. A missing or
/// tombstoned ancestor is not an error — the corresponding block is
/// just omitted.
pub fn collect_inherited_blocks(
    storage: &SqliteStorage,
    child_id: &str,
) -> Result<Vec<InheritedBlock>> {
    let Some(immediate_parent_id) = find_immediate_parent_id(storage, child_id)? else {
        return Ok(Vec::new());
    };

    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(child_id.to_string());

    // Parent was tombstoned/deleted/missing — treat as if the child has
    // no parent, per spec ("silently skipped").
    let Some(immediate_parent) = load_active_issue(storage, &immediate_parent_id)? else {
        return Ok(Vec::new());
    };
    visited.insert(immediate_parent.id.clone());

    let root = walk_to_root(storage, &immediate_parent, &mut visited)?;

    let mut blocks = Vec::new();
    let mut emitted_ids: HashSet<String> = HashSet::new();

    if let Some(root_issue) = root.as_ref() {
        // Emit root first so the closer ancestor (parent) sits adjacent
        // to the child output, matching the spec's text-mode layout.
        if !emitted_ids.contains(&root_issue.id)
            && root_issue.id != immediate_parent.id
            && let Some(block) = block_from_ancestor(root_issue, root_role(root_issue))
        {
            emitted_ids.insert(root_issue.id.clone());
            blocks.push(block);
        }
    }

    // Choose the most informative role for the immediate parent. When
    // the parent is also the root (single-block emission), upgrade the
    // label to "epic" if the parent is an epic — that's the case where
    // labeling it as merely "parent" hides the strategic context the
    // emission feature is supposed to surface. In the multi-block case
    // the root block already carries the "epic" label so the parent
    // block correctly stays "parent".
    let parent_is_also_root = root.as_ref().is_some_and(|r| r.id == immediate_parent.id);
    let parent_role = if parent_is_also_root
        && storage
            .issue_type_registry()
            .capabilities_for_name(immediate_parent.issue_type.as_str())
            .inherited_context_root
    {
        root_role(&immediate_parent)
    } else {
        "parent"
    };

    if !emitted_ids.contains(&immediate_parent.id)
        && let Some(block) = block_from_ancestor(&immediate_parent, parent_role)
    {
        emitted_ids.insert(immediate_parent.id.clone());
        blocks.push(block);
    }

    Ok(blocks)
}

fn root_role(issue: &Issue) -> &'static str {
    if matches!(issue.issue_type, IssueType::Epic) {
        "epic"
    } else {
        "root"
    }
}

fn block_from_ancestor(issue: &Issue, role: &str) -> Option<InheritedBlock> {
    let content = issue.agent_context.as_deref()?.trim();
    if content.is_empty() {
        return None;
    }
    Some(InheritedBlock {
        source_id: issue.id.clone(),
        source_role: role.to_string(),
        source_title: issue.title.clone(),
        field_used: "agent_context".to_string(),
        content: content.to_string(),
    })
}

fn find_immediate_parent_id(storage: &SqliteStorage, child_id: &str) -> Result<Option<String>> {
    // Beads uses dependency rows to encode parent relationships. The
    // canonical dependency type used by the bd ecosystem is
    // `ParentChild` with the child as the dependent and the parent as
    // the depends_on target. We use `get_dependencies_full` instead of
    // touching the connection directly because the storage layer's
    // RefCell-backed prepared-statement cache is private to the
    // SqliteStorage abstraction.
    let deps = storage.get_dependencies_full(child_id)?;
    Ok(deps
        .into_iter()
        .find(|d| matches!(d.dep_type, crate::model::DependencyType::ParentChild))
        .map(|d| d.depends_on_id))
}

fn walk_to_root(
    storage: &SqliteStorage,
    start: &Issue,
    visited: &mut HashSet<String>,
) -> Result<Option<Issue>> {
    let mut current = start.clone();
    let prefer_configured_root = !storage.issue_type_registry().is_builtin_only();
    // Configured roots use nearest-ancestor semantics. The built-in-only
    // branch retains the historical topmost-Epic behavior exactly.
    let mut nearest_configured_root: Option<Issue> = if prefer_configured_root
        && storage
            .issue_type_registry()
            .capabilities_for_name(current.issue_type.as_str())
            .inherited_context_root
    {
        Some(current.clone())
    } else {
        None
    };
    let mut latest_epic: Option<Issue> = if matches!(current.issue_type, IssueType::Epic) {
        Some(current.clone())
    } else {
        None
    };

    loop {
        // current is the terminal ancestor if it has no parent.
        let Some(parent_id) = find_immediate_parent_id(storage, &current.id)? else {
            break;
        };
        if !visited.insert(parent_id.clone()) {
            eprintln!(
                "br: inheritance: detected cycle in parent chain at {parent_id}; \
                 stopping ancestor traversal"
            );
            break;
        }
        // Tombstoned / missing — terminate the walk; whatever was most
        // recently loaded is the best root candidate we have.
        let Some(parent) = load_active_issue(storage, &parent_id)? else {
            break;
        };
        if matches!(parent.issue_type, IssueType::Epic) {
            latest_epic = Some(parent.clone());
        }
        if prefer_configured_root
            && nearest_configured_root.is_none()
            && storage
                .issue_type_registry()
                .capabilities_for_name(parent.issue_type.as_str())
                .inherited_context_root
        {
            nearest_configured_root = Some(parent.clone());
        }
        current = parent;
    }

    if prefer_configured_root {
        Ok(nearest_configured_root.or(Some(current)))
    } else {
        // Historical behavior: prefer the topmost Epic (if any) over the
        // chain's terminal ancestor.
        Ok(latest_epic.or(Some(current)))
    }
}

fn load_active_issue(storage: &SqliteStorage, id: &str) -> Result<Option<Issue>> {
    let issue = storage.get_issue(id)?;
    let Some(issue) = issue else { return Ok(None) };
    // Skip tombstoned/deleted ancestors silently.
    if matches!(issue.status, crate::model::Status::Tombstone) || issue.deleted_at.is_some() {
        return Ok(None);
    }
    Ok(Some(issue))
}

/// Render the inherited blocks as the canonical text-mode prefix that
/// precedes a `br show` / `br update --status in_progress` output. The
/// blocks are emitted root-first so the immediate parent sits adjacent
/// to the child's content.
#[must_use]
pub fn render_text(blocks: &[InheritedBlock]) -> String {
    if blocks.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for block in blocks {
        let role = match block.source_role.as_str() {
            "epic" => format!("epic {}", block.source_id),
            "root" => format!("root ancestor {}", block.source_id),
            "parent" => format!("parent {}", block.source_id),
            other => format!("{other} {}", block.source_id),
        };
        out.push_str(&format!(
            "--- Inherited context (from {role}: {title:?}) ---\n",
            title = block.source_title
        ));
        // Try to pretty-print JSON content; fall back to raw on parse
        // failure (the data is opaque TEXT so we can't assume validity).
        let pretty = serde_json::from_str::<serde_json::Value>(&block.content)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or_else(|| block.content.clone());
        out.push_str(&pretty);
        if !pretty.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!(
            "--- End inherited context ({}) ---\n",
            block.source_id
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Dependency, DependencyType, Priority, Status};
    use chrono::Utc;

    fn test_issue(id: &str, issue_type: IssueType, agent_context: Option<&str>) -> Issue {
        let now = Utc::now();
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: id.to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: now,
            created_by: None,
            updated_at: now,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            source_repo_path: None,
            agent_context: agent_context.map(str::to_string),
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: Vec::new(),
            dependencies: Vec::new(),
            comments: Vec::new(),
        }
    }

    fn parent_dependency(child: &str, parent: &str) -> Dependency {
        Dependency {
            issue_id: child.to_string(),
            depends_on_id: parent.to_string(),
            dep_type: DependencyType::ParentChild,
            created_at: Utc::now(),
            created_by: Some("tester".to_string()),
            metadata: None,
            thread_id: None,
        }
    }

    fn install_map_context_root(storage: &mut SqliteStorage) {
        let policy = crate::close_policy::IssueTypePolicy {
            types: vec![crate::close_policy::IssueTypeDefinition {
                name: "map".to_string(),
                capabilities: crate::close_policy::IssueTypeCapabilityOverrides {
                    inherited_context_root: Some(true),
                    ..Default::default()
                },
                roadmap_role: None,
            }],
            ..Default::default()
        };
        storage.set_issue_type_registry(
            crate::close_policy::TypeCapabilityRegistry::resolve(&policy).unwrap(),
        );
    }

    #[test]
    fn render_text_emits_empty_string_for_no_blocks() {
        assert_eq!(render_text(&[]), "");
    }

    #[test]
    fn render_text_emits_blocks_in_order_with_role_labels() {
        let blocks = vec![
            InheritedBlock {
                source_id: "bd-epic".into(),
                source_role: "epic".into(),
                source_title: "Auth rewrite".into(),
                field_used: "agent_context".into(),
                content: "{\"skills\":[\"clean-code\"]}".into(),
            },
            InheritedBlock {
                source_id: "bd-parent".into(),
                source_role: "parent".into(),
                source_title: "Token refresh".into(),
                field_used: "agent_context".into(),
                content: "{\"constraints\":[\"backwards-compatible\"]}".into(),
            },
        ];
        let out = render_text(&blocks);
        let epic_idx = out.find("epic bd-epic").expect("epic label present");
        let parent_idx = out.find("parent bd-parent").expect("parent label present");
        assert!(epic_idx < parent_idx, "root/epic must precede parent");
        assert!(out.contains("Auth rewrite"));
        assert!(out.contains("clean-code"));
        assert!(out.contains("backwards-compatible"));
    }

    #[test]
    fn render_text_falls_back_to_raw_on_malformed_json() {
        let blocks = vec![InheritedBlock {
            source_id: "bd-x".into(),
            source_role: "parent".into(),
            source_title: "Title".into(),
            field_used: "agent_context".into(),
            content: "not-json-but-still-shown".into(),
        }];
        let out = render_text(&blocks);
        assert!(out.contains("not-json-but-still-shown"));
    }

    #[test]
    fn is_enabled_respects_env_var_truthy_values() {
        let missing_config = std::path::Path::new("/nonexistent/config.yaml");
        assert!(is_enabled_from_inputs(Some("1"), missing_config));
        assert!(is_enabled_from_inputs(Some("true"), missing_config));
        assert!(is_enabled_from_inputs(Some("TRUE"), missing_config));
        assert!(is_enabled_from_inputs(Some("yes"), missing_config));
        assert!(is_enabled_from_inputs(
            Some("anything-non-empty"),
            missing_config
        ));
        assert!(!is_enabled_from_inputs(Some("0"), missing_config));
        assert!(!is_enabled_from_inputs(Some("false"), missing_config));
        assert!(!is_enabled_from_inputs(Some("FALSE"), missing_config));
        assert!(!is_enabled_from_inputs(Some("no"), missing_config));
        assert!(!is_enabled_from_inputs(Some(""), missing_config));
        // Missing env var + missing config => disabled (the safe default).
        assert!(!is_enabled_from_inputs(None, missing_config));
    }

    #[test]
    fn is_enabled_reads_config_yaml_when_env_var_unset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("config.yaml");
        // Missing file => disabled.
        assert!(!is_enabled_from_inputs(None, &cfg));

        std::fs::write(&cfg, "inherited_context:\n  enabled: true\n")
            .expect("write enabled config");
        assert!(is_enabled_from_inputs(None, &cfg));

        std::fs::write(&cfg, "inherited_context:\n  enabled: false\n")
            .expect("write disabled config");
        assert!(!is_enabled_from_inputs(None, &cfg));

        // No inherited_context section at all => disabled (back-compat
        // for projects that haven't opted in).
        std::fs::write(&cfg, "issue_prefix: bd\n").expect("write plain config");
        assert!(!is_enabled_from_inputs(None, &cfg));

        // Malformed YAML => disabled (don't fail the show on bad config).
        std::fs::write(&cfg, "this: is: not: yaml: [").expect("write malformed");
        assert!(!is_enabled_from_inputs(None, &cfg));
    }

    #[test]
    fn configured_context_root_prefers_nearest_ancestor_and_stops_on_cycle() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        install_map_context_root(&mut storage);
        for issue in [
            test_issue(
                "bd-outer",
                IssueType::Custom("map".to_string()),
                Some("outer context"),
            ),
            test_issue(
                "bd-inner",
                IssueType::Custom("map".to_string()),
                Some("inner context"),
            ),
            test_issue("bd-leaf", IssueType::Task, None),
        ] {
            storage.create_issue(&issue, "tester").unwrap();
        }
        storage
            .sync_dependencies_for_import("bd-leaf", &[parent_dependency("bd-leaf", "bd-inner")])
            .unwrap();
        storage
            .sync_dependencies_for_import("bd-inner", &[parent_dependency("bd-inner", "bd-outer")])
            .unwrap();
        storage
            .sync_dependencies_for_import("bd-outer", &[parent_dependency("bd-outer", "bd-inner")])
            .unwrap();

        let blocks = collect_inherited_blocks(&storage, "bd-leaf").unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].source_id, "bd-inner");
        assert_eq!(blocks[0].source_role, "root");
        assert_eq!(blocks[0].content, "inner context");
    }

    #[test]
    fn missing_parent_is_silently_ignored() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        storage
            .create_issue(&test_issue("bd-orphan", IssueType::Task, None), "tester")
            .unwrap();
        storage
            .sync_dependencies_for_import(
                "bd-orphan",
                &[parent_dependency("bd-orphan", "bd-missing")],
            )
            .unwrap();
        assert!(
            collect_inherited_blocks(&storage, "bd-orphan")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn builtin_only_registry_preserves_topmost_epic_preference() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        for issue in [
            test_issue("bd-outer", IssueType::Epic, Some("outer epic")),
            test_issue("bd-inner", IssueType::Epic, Some("inner epic")),
            test_issue("bd-leaf", IssueType::Task, None),
        ] {
            storage.create_issue(&issue, "tester").unwrap();
        }
        storage
            .add_dependency("bd-inner", "bd-outer", "parent-child", "tester")
            .unwrap();
        storage
            .add_dependency("bd-leaf", "bd-inner", "parent-child", "tester")
            .unwrap();

        let blocks = collect_inherited_blocks(&storage, "bd-leaf").unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].source_id, "bd-outer");
        assert_eq!(blocks[0].source_role, "epic");
        assert_eq!(blocks[1].source_id, "bd-inner");
        assert_eq!(blocks[1].source_role, "parent");
    }

    #[test]
    fn configured_registry_without_preferred_root_falls_back_to_terminal_ancestor() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        install_map_context_root(&mut storage);
        for issue in [
            test_issue("bd-root", IssueType::Task, Some("terminal context")),
            test_issue("bd-middle", IssueType::Task, None),
            test_issue("bd-leaf", IssueType::Task, None),
        ] {
            storage.create_issue(&issue, "tester").unwrap();
        }
        storage
            .add_dependency("bd-middle", "bd-root", "parent-child", "tester")
            .unwrap();
        storage
            .add_dependency("bd-leaf", "bd-middle", "parent-child", "tester")
            .unwrap();

        let blocks = collect_inherited_blocks(&storage, "bd-leaf").unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].source_id, "bd-root");
        assert_eq!(blocks[0].source_role, "root");
        assert_eq!(blocks[0].content, "terminal context");
    }
}
