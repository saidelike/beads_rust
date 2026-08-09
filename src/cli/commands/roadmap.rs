//! Composite containment and traceability roadmap view.

use crate::cli::{RoadmapArgs, RoadmapFocus, RoadmapOutputFormat, RoadmapSection};
use crate::close_policy::{RoadmapRole, TypeCapabilityRegistry};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::{DependencyType, Issue, IssueType, Priority, Status};
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::util::id::{IdResolver, ResolverConfig};
use rich_rust::prelude::{Panel, Text};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::num::NonZeroUsize;

const CONTRACT_VERSION: &str = "br.roadmap.v1";

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RoadmapOutput {
    contract_version: &'static str,
    requested_root: RequestedRoot,
    roots: Vec<String>,
    nodes: Vec<RoadmapNode>,
    edges: Vec<RoadmapEdge>,
    placements: Vec<RoadmapPlacement>,
    filters: RoadmapFilters,
    truncation: RoadmapTruncation,
    summaries: Vec<RoadmapSummary>,
    diagnostics: Vec<RoadmapDiagnostic>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct RequestedRoot {
    id: String,
    marked: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct RoadmapNode {
    id: String,
    title: String,
    issue_type: String,
    status: String,
    priority: i32,
    is_template: bool,
    missing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    roadmap_role: Option<RoadmapRole>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct RoadmapEdge {
    from: String,
    to: String,
    relation: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct RoadmapPlacement {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    relation: Option<String>,
    depth: usize,
    requested: bool,
    reference: bool,
    truncated: bool,
    /// This truncated placement is on an upstream path to the requested issue.
    requested_route: bool,
    hidden_count: usize,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct RoadmapFilters {
    focus: Vec<String>,
    path_compression: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct RoadmapTruncation {
    max_depth: Option<usize>,
    truncated_placements: usize,
    requested_hidden: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct RoadmapDiagnostic {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    issue_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct RoadmapSummary {
    id: String,
    title: String,
    scope: &'static str,
    rows: Vec<RoadmapSummaryRow>,
    total: RoadmapProgress,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct RoadmapSummaryRow {
    issue_type: String,
    count: usize,
    terminal: usize,
    completion_percent: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
struct RoadmapProgress {
    count: usize,
    terminal: usize,
    completion_percent: Option<usize>,
}

#[derive(Debug, Clone)]
struct DisplayEdge {
    from: String,
    to: String,
    relation: DependencyType,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SortKey {
    Priority,
    Status,
    Title,
    Id,
}

#[derive(Debug, Clone, Copy)]
struct SortTerm {
    key: SortKey,
    descending: bool,
}

/// Execute `br roadmap` using an already-open storage handle when available.
///
/// # Errors
///
/// Returns an error for invalid arguments, missing roots, policy failures, or
/// unreadable storage.
pub fn execute(
    args: &RoadmapArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    storage_ctx: Option<&config::OpenStorageResult>,
) -> Result<()> {
    if let Some(storage_ctx) = storage_ctx {
        return execute_with_storage(args, cli, ctx, storage_ctx);
    }
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    execute_with_storage(args, cli, ctx, &storage_ctx)
}

fn execute_with_storage(
    args: &RoadmapArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    storage_ctx: &config::OpenStorageResult,
) -> Result<()> {
    let format = resolve_format(args, cli, ctx)?;
    if matches!(
        format,
        RoadmapOutputFormat::Mermaid | RoadmapOutputFormat::Dot
    ) && args.only == Some(RoadmapSection::Summary)
    {
        return Err(BeadsError::Validation {
            field: "only".to_string(),
            reason: "summary-only output is not supported with mermaid or dot".to_string(),
        });
    }
    let sort = parse_sort(&args.sort)?;
    let config_layer = storage_ctx.load_config(cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let requested = super::resolve_issue_id(&storage_ctx.storage, &resolver, &args.root)?;
    let output = build_output(
        args,
        &storage_ctx.storage,
        &requested,
        storage_ctx.storage.issue_type_registry(),
        &sort,
    )?;
    if !ctx.is_quiet() {
        render_output(&output, args, format, ctx);
    }
    Ok(())
}

fn resolve_format(
    args: &RoadmapArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<RoadmapOutputFormat> {
    let environment_format = crate::cli::OutputFormat::from_env();
    if args.format.is_some() && (cli.json.unwrap_or(false) || environment_format.is_some()) {
        return Err(BeadsError::Validation {
            field: "format".to_string(),
            reason: "global or environment output selection conflicts with roadmap --format"
                .to_string(),
        });
    }
    if let Some(format) = args.format {
        return Ok(format);
    }
    if matches!(environment_format, Some(crate::cli::OutputFormat::Toon)) || ctx.is_toon() {
        return Err(BeadsError::Validation {
            field: "format".to_string(),
            reason: "roadmap does not support TOON output in v1".to_string(),
        });
    }
    if matches!(environment_format, Some(crate::cli::OutputFormat::Csv)) {
        return Err(BeadsError::Validation {
            field: "format".to_string(),
            reason: "roadmap does not support CSV output".to_string(),
        });
    }
    Ok(if ctx.is_json() {
        RoadmapOutputFormat::Json
    } else {
        RoadmapOutputFormat::Text
    })
}

#[allow(clippy::too_many_lines)]
fn build_output(
    args: &RoadmapArgs,
    storage: &SqliteStorage,
    requested: &str,
    registry: &TypeCapabilityRegistry,
    sort: &[SortTerm],
) -> Result<RoadmapOutput> {
    let issue_ids: Vec<String> = storage
        .get_all_issues_metadata()?
        .into_iter()
        .map(|metadata| metadata.id)
        .collect();
    let issues: BTreeMap<String, Issue> = storage
        .get_issues_by_ids(&issue_ids)?
        .into_iter()
        .map(|issue| (issue.id.clone(), issue))
        .collect();
    if !issues.contains_key(requested) {
        return Err(BeadsError::IssueNotFound {
            id: requested.to_string(),
        });
    }

    let mut all_edges = Vec::new();
    let mut diagnostics = Vec::new();
    for dependencies in storage.get_all_dependency_records()?.into_values() {
        for dependency in dependencies {
            if matches!(
                dependency.dep_type,
                DependencyType::ParentChild
                    | DependencyType::DerivedFrom
                    | DependencyType::Implements
            ) {
                if !issues.contains_key(&dependency.issue_id) {
                    diagnostics.push(RoadmapDiagnostic {
                        code: "missing_endpoint",
                        message: format!(
                            "{} relation references missing artifact '{}'",
                            dependency.dep_type, dependency.issue_id
                        ),
                        issue_id: Some(dependency.issue_id.clone()),
                    });
                }
                if !issues.contains_key(&dependency.depends_on_id) {
                    diagnostics.push(RoadmapDiagnostic {
                        code: "missing_endpoint",
                        message: format!(
                            "{} relation from '{}' references missing source '{}'",
                            dependency.dep_type, dependency.issue_id, dependency.depends_on_id
                        ),
                        issue_id: Some(dependency.issue_id.clone()),
                    });
                }
                all_edges.push(DisplayEdge {
                    from: dependency.depends_on_id,
                    to: dependency.issue_id,
                    relation: dependency.dep_type,
                });
            }
        }
    }

    let mut parents: HashMap<String, Vec<DisplayEdge>> = HashMap::new();
    let mut children: HashMap<String, Vec<DisplayEdge>> = HashMap::new();
    for edge in &all_edges {
        parents
            .entry(edge.to.clone())
            .or_default()
            .push(edge.clone());
        children
            .entry(edge.from.clone())
            .or_default()
            .push(edge.clone());
    }

    let ancestors = reachable_upstream(requested, &parents);
    let mut roots = select_roots(&ancestors, &parents, &issues, registry);
    if roots.is_empty() {
        roots.push(requested.to_string());
        diagnostics.push(RoadmapDiagnostic {
            code: "cycle_root",
            message: "upstream traceability contains a cycle; using requested issue as root"
                .to_string(),
            issue_id: Some(requested.to_string()),
        });
    }
    if ancestors.len() == 1
        && matches!(
            registry.roadmap_role_for_name(issues[requested].issue_type.as_str()),
            Some(RoadmapRole::Specification | RoadmapRole::Implementation)
        )
    {
        diagnostics.push(RoadmapDiagnostic {
            code: "unlinked_role_root",
            message: format!(
                "requested role-bearing issue '{requested}' has no stored roadmap authority link"
            ),
            issue_id: Some(requested.to_string()),
        });
    }
    roots.sort_by(|left, right| compare_issues(left, right, &issues, sort));

    let reachable = reachable_downstream(&roots, &children);
    let focus = Focus::from_args(&args.focus);
    if focus.is_focused()
        && registry
            .roadmap_role_for_name(issues[requested].issue_type.as_str())
            .is_none()
    {
        diagnostics.push(RoadmapDiagnostic {
            code: "unclassified_requested_root",
            message: format!(
                "requested issue '{requested}' has no configured roadmap role; omitted from focused placements"
            ),
            issue_id: Some(requested.to_string()),
        });
    }

    for edges in children.values_mut() {
        edges.sort_by(|left, right| {
            relation_rank(&left.relation)
                .cmp(&relation_rank(&right.relation))
                .then_with(|| compare_issues(&left.to, &right.to, &issues, sort))
        });
    }

    let mut placements = Vec::new();
    let mut expanded = HashSet::new();
    let mut path = Vec::new();
    let build = PlacementBuild {
        requested,
        requested_ancestry: &ancestors,
        issues: &issues,
        children: &children,
        registry,
        focus,
        max_depth: args.max_depth,
    };
    for root in &roots {
        build_placement(
            root,
            None,
            None,
            0,
            0,
            &build,
            &mut expanded,
            &mut path,
            &mut placements,
            &mut diagnostics,
        );
    }

    let visible_ids: BTreeSet<String> = placements.iter().map(|item| item.id.clone()).collect();
    let mut nodes: Vec<RoadmapNode> = reachable
        .iter()
        .map(|id| {
            issues.get(id).map_or_else(
                || RoadmapNode {
                    id: id.clone(),
                    title: "[missing issue]".to_string(),
                    issue_type: "missing".to_string(),
                    status: "missing".to_string(),
                    priority: Priority::MEDIUM.0,
                    is_template: false,
                    missing: true,
                    roadmap_role: None,
                },
                |issue| RoadmapNode {
                    id: issue.id.clone(),
                    title: issue.title.clone(),
                    issue_type: issue.issue_type.as_str().to_string(),
                    status: issue.status.as_str().to_string(),
                    priority: issue.priority.0,
                    is_template: issue.is_template,
                    missing: false,
                    roadmap_role: registry.roadmap_role_for_name(issue.issue_type.as_str()),
                },
            )
        })
        .collect();
    nodes.sort_by(|left, right| left.id.cmp(&right.id));

    let mut edges: Vec<RoadmapEdge> = all_edges
        .iter()
        .filter(|edge| reachable.contains(&edge.from) || reachable.contains(&edge.to))
        .map(|edge| RoadmapEdge {
            from: edge.from.clone(),
            to: edge.to.clone(),
            relation: edge.relation.as_str().to_string(),
        })
        .collect();
    edges.sort_by(|left, right| {
        relation_name_rank(&left.relation)
            .cmp(&relation_name_rank(&right.relation))
            .then_with(|| left.from.cmp(&right.from))
            .then_with(|| left.to.cmp(&right.to))
    });

    if focus.is_focused() {
        let omitted = reachable
            .iter()
            .filter(|id| issues.contains_key(*id) && !visible_ids.contains(*id))
            .count();
        if omitted > 0 {
            diagnostics.push(RoadmapDiagnostic {
                code: "focused_nodes_omitted",
                message: format!(
                    "focused roadmap omitted {omitted} role-hidden or unclassified node(s); paths were compressed"
                ),
                issue_id: None,
            });
        }
    }

    diagnostics.sort_by(|left, right| {
        left.code
            .cmp(right.code)
            .then_with(|| left.issue_id.cmp(&right.issue_id))
            .then_with(|| left.message.cmp(&right.message))
    });
    diagnostics.dedup_by(|left, right| {
        left.code == right.code && left.issue_id == right.issue_id && left.message == right.message
    });

    let summaries = if args.only != Some(RoadmapSection::Graph) {
        let summary_build = PlacementBuild {
            requested,
            requested_ancestry: &ancestors,
            issues: &issues,
            children: &children,
            registry,
            focus,
            max_depth: None,
        };
        let mut summary_placements = Vec::new();
        let mut summary_expanded = HashSet::new();
        let mut summary_path = Vec::new();
        let mut summary_diagnostics = Vec::new();
        for root in &roots {
            build_placement(
                root,
                None,
                None,
                0,
                0,
                &summary_build,
                &mut summary_expanded,
                &mut summary_path,
                &mut summary_placements,
                &mut summary_diagnostics,
            );
        }
        build_summaries(&summary_placements, &issues, registry)
    } else {
        Vec::new()
    };
    let truncated_placements = placements.iter().filter(|item| item.truncated).count();
    let requested_marked = placements.iter().any(|item| item.requested);

    Ok(RoadmapOutput {
        contract_version: CONTRACT_VERSION,
        requested_root: RequestedRoot {
            id: requested.to_string(),
            marked: requested_marked,
        },
        roots: if args.only == Some(RoadmapSection::Summary) {
            Vec::new()
        } else {
            roots
        },
        nodes: if args.only == Some(RoadmapSection::Summary) {
            Vec::new()
        } else {
            nodes
        },
        edges: if args.only == Some(RoadmapSection::Summary) {
            Vec::new()
        } else {
            edges
        },
        placements: if args.only == Some(RoadmapSection::Summary) {
            Vec::new()
        } else {
            placements
        },
        filters: RoadmapFilters {
            focus: args
                .focus
                .iter()
                .map(|focus| match focus {
                    RoadmapFocus::Decisions => "decisions".to_string(),
                    RoadmapFocus::Implementations => "implementations".to_string(),
                })
                .collect(),
            path_compression: focus.is_focused(),
        },
        truncation: RoadmapTruncation {
            max_depth: args.max_depth,
            truncated_placements,
            requested_hidden: !requested_marked,
        },
        summaries,
        diagnostics,
    })
}

#[derive(Clone, Copy)]
enum Focus {
    All,
    Decisions,
    Implementations,
}

impl Focus {
    fn from_args(values: &[RoadmapFocus]) -> Self {
        let decisions = values.contains(&RoadmapFocus::Decisions);
        let implementations = values.contains(&RoadmapFocus::Implementations);
        match (decisions, implementations) {
            (true, false) => Self::Decisions,
            (false, true) => Self::Implementations,
            _ => Self::All,
        }
    }

    const fn is_focused(self) -> bool {
        !matches!(self, Self::All)
    }

    const fn includes(self, role: Option<RoadmapRole>) -> bool {
        match self {
            Self::All => true,
            Self::Decisions => matches!(role, Some(RoadmapRole::Authority | RoadmapRole::Decision)),
            Self::Implementations => matches!(
                role,
                Some(
                    RoadmapRole::Authority
                        | RoadmapRole::Specification
                        | RoadmapRole::Implementation
                )
            ),
        }
    }
}

struct PlacementBuild<'a> {
    requested: &'a str,
    requested_ancestry: &'a BTreeSet<String>,
    issues: &'a BTreeMap<String, Issue>,
    children: &'a HashMap<String, Vec<DisplayEdge>>,
    registry: &'a TypeCapabilityRegistry,
    focus: Focus,
    max_depth: Option<usize>,
}

#[allow(clippy::too_many_arguments)]
fn build_placement(
    id: &str,
    parent_id: Option<&str>,
    relation: Option<&DependencyType>,
    depth: usize,
    hidden_count: usize,
    build: &PlacementBuild<'_>,
    expanded: &mut HashSet<String>,
    path: &mut Vec<String>,
    placements: &mut Vec<RoadmapPlacement>,
    diagnostics: &mut Vec<RoadmapDiagnostic>,
) {
    let visible = build.issues.get(id).map_or_else(
        || matches!(build.focus, Focus::All),
        |issue| {
            build.focus.includes(
                build
                    .registry
                    .roadmap_role_for_name(issue.issue_type.as_str()),
            )
        },
    );
    if !visible {
        for edge in build.children.get(id).into_iter().flatten() {
            build_placement(
                &edge.to,
                parent_id,
                Some(&edge.relation),
                depth,
                hidden_count + 1,
                build,
                expanded,
                path,
                placements,
                diagnostics,
            );
        }
        return;
    }

    let reference = !expanded.insert(id.to_string());
    if reference {
        diagnostics.push(RoadmapDiagnostic {
            code: "shared_reference",
            message: format!("shared roadmap node '{id}' is expanded once and referenced here"),
            issue_id: Some(id.to_string()),
        });
    }
    let has_children = build
        .children
        .get(id)
        .is_some_and(|children| !children.is_empty());
    let truncated = !reference && build.max_depth.is_some_and(|max| depth >= max) && has_children;
    placements.push(RoadmapPlacement {
        id: id.to_string(),
        parent_id: parent_id.map(str::to_string),
        relation: relation.map(|relation| relation.as_str().to_string()),
        depth,
        requested: id == build.requested,
        reference,
        truncated,
        requested_route: truncated && build.requested_ancestry.contains(id),
        hidden_count,
    });

    if reference || truncated {
        return;
    }
    if path.iter().any(|ancestor| ancestor == id) {
        diagnostics.push(RoadmapDiagnostic {
            code: "cycle",
            message: format!("roadmap cycle encountered at '{id}'"),
            issue_id: Some(id.to_string()),
        });
        return;
    }
    path.push(id.to_string());
    for edge in build.children.get(id).into_iter().flatten() {
        if path.iter().any(|ancestor| ancestor == &edge.to) {
            diagnostics.push(RoadmapDiagnostic {
                code: "cycle",
                message: format!("roadmap cycle encountered on {} -> {}", edge.from, edge.to),
                issue_id: Some(edge.to.clone()),
            });
            continue;
        }
        build_placement(
            &edge.to,
            Some(id),
            Some(&edge.relation),
            depth + 1,
            0,
            build,
            expanded,
            path,
            placements,
            diagnostics,
        );
    }
    path.pop();
}

fn build_summaries(
    placements: &[RoadmapPlacement],
    issues: &BTreeMap<String, Issue>,
    registry: &TypeCapabilityRegistry,
) -> Vec<RoadmapSummary> {
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for placement in placements {
        if let Some(parent) = &placement.parent_id {
            children
                .entry(parent.clone())
                .or_default()
                .push(placement.id.clone());
        }
    }
    let mut scope_ids = Vec::new();
    let mut seen_scopes = HashSet::new();
    for placement in placements {
        if placement.reference {
            continue;
        }
        if let Some(issue) = issues.get(&placement.id)
            && (children.contains_key(&placement.id)
                || registry
                    .capabilities_for_name(issue.issue_type.as_str())
                    .aggregate
                || matches!(
                    registry.roadmap_role_for_name(issue.issue_type.as_str()),
                    Some(RoadmapRole::Authority | RoadmapRole::Specification)
                ))
            && seen_scopes.insert(placement.id.clone())
        {
            scope_ids.push(placement.id.clone());
        }
    }
    scope_ids
        .into_iter()
        .filter_map(|id| {
            let child_ids = children.remove(&id).unwrap_or_default();
            let issue = issues.get(&id)?;
            let mut by_type: BTreeMap<String, (usize, usize)> = BTreeMap::new();
            for child_id in child_ids {
                let Some(child) = issues.get(&child_id) else {
                    continue;
                };
                if child.is_template {
                    continue;
                }
                let entry = by_type
                    .entry(child.issue_type.as_str().to_string())
                    .or_default();
                entry.0 += 1;
                entry.1 += usize::from(child.status.is_terminal());
            }
            let type_order = registry.type_order();
            let mut rows: Vec<RoadmapSummaryRow> = by_type
                .into_iter()
                .map(|(issue_type, (count, terminal))| RoadmapSummaryRow {
                    issue_type,
                    count,
                    terminal,
                    completion_percent: percent(terminal, count),
                })
                .collect();
            rows.sort_by(|left, right| {
                type_order
                    .iter()
                    .position(|name| name == &left.issue_type)
                    .unwrap_or(usize::MAX)
                    .cmp(
                        &type_order
                            .iter()
                            .position(|name| name == &right.issue_type)
                            .unwrap_or(usize::MAX),
                    )
                    .then_with(|| left.issue_type.cmp(&right.issue_type))
            });
            let count = rows.iter().map(|row| row.count).sum();
            let terminal = rows.iter().map(|row| row.terminal).sum();
            Some(RoadmapSummary {
                id,
                title: issue.title.clone(),
                scope: "directly related issues",
                rows,
                total: RoadmapProgress {
                    count,
                    terminal,
                    completion_percent: percent(terminal, count),
                },
            })
        })
        .collect()
}

fn percent(terminal: usize, count: usize) -> Option<usize> {
    let count = NonZeroUsize::new(count)?;
    Some((terminal * 100 + count.get() / 2) / count.get())
}

fn select_roots(
    ancestors: &BTreeSet<String>,
    parents: &HashMap<String, Vec<DisplayEdge>>,
    issues: &BTreeMap<String, Issue>,
    registry: &TypeCapabilityRegistry,
) -> Vec<String> {
    let authority_ids: BTreeSet<String> = ancestors
        .iter()
        .filter(|id| {
            issues.get(*id).is_some_and(|issue| {
                registry.roadmap_role_for_name(issue.issue_type.as_str())
                    == Some(RoadmapRole::Authority)
            })
        })
        .cloned()
        .collect();

    if !authority_ids.is_empty() {
        return authority_ids
            .iter()
            .filter(|authority| {
                !reachable_upstream(authority, parents)
                    .iter()
                    .any(|upstream| upstream != *authority && authority_ids.contains(upstream))
            })
            .cloned()
            .collect();
    }

    ancestors
        .iter()
        .filter(|id| {
            parents
                .get(*id)
                .is_none_or(|incoming| incoming.iter().all(|edge| !ancestors.contains(&edge.from)))
        })
        .cloned()
        .collect()
}

fn reachable_upstream(
    requested: &str,
    parents: &HashMap<String, Vec<DisplayEdge>>,
) -> BTreeSet<String> {
    let mut reachable = BTreeSet::new();
    let mut frontier = vec![requested.to_string()];
    while let Some(id) = frontier.pop() {
        if !reachable.insert(id.clone()) {
            continue;
        }
        frontier.extend(
            parents
                .get(&id)
                .into_iter()
                .flatten()
                .map(|edge| edge.from.clone()),
        );
    }
    reachable
}

fn reachable_downstream(
    roots: &[String],
    children: &HashMap<String, Vec<DisplayEdge>>,
) -> BTreeSet<String> {
    let mut reachable = BTreeSet::new();
    let mut frontier = roots.to_vec();
    while let Some(id) = frontier.pop() {
        if !reachable.insert(id.clone()) {
            continue;
        }
        frontier.extend(
            children
                .get(&id)
                .into_iter()
                .flatten()
                .map(|edge| edge.to.clone()),
        );
    }
    reachable
}

fn relation_rank(relation: &DependencyType) -> usize {
    relation_name_rank(relation.as_str())
}

fn relation_name_rank(relation: &str) -> usize {
    match relation {
        "parent-child" => 0,
        "derived-from" => 1,
        "implements" => 2,
        _ => 3,
    }
}

fn parse_sort(raw: &str) -> Result<Vec<SortTerm>> {
    let mut seen = HashSet::new();
    let mut terms = Vec::new();
    for raw_term in raw.split(',') {
        let raw_term = raw_term.trim();
        let (prefixed_desc, body) = match raw_term.as_bytes().first() {
            Some(b'-') => (true, &raw_term[1..]),
            Some(b'+') => (false, &raw_term[1..]),
            _ => (false, raw_term),
        };
        let mut pieces = body.split(':');
        let name = pieces.next().unwrap_or_default();
        let direction = pieces.next();
        if pieces.next().is_some() || !seen.insert(name) {
            return Err(BeadsError::Validation {
                field: "sort".to_string(),
                reason: format!("invalid or duplicate roadmap sort term '{raw_term}'"),
            });
        }
        if direction.is_some() && raw_term.len() != body.len() {
            return Err(BeadsError::Validation {
                field: "sort".to_string(),
                reason: format!(
                    "roadmap sort term '{raw_term}' mixes prefix and :direction syntax"
                ),
            });
        }
        let key = match name {
            "priority" => SortKey::Priority,
            "status" => SortKey::Status,
            "title" => SortKey::Title,
            "id" => SortKey::Id,
            _ => {
                return Err(BeadsError::Validation {
                    field: "sort".to_string(),
                    reason: format!("invalid key '{name}'; expected priority,status,title,id"),
                });
            }
        };
        let descending = match direction {
            None | Some("asc") => prefixed_desc,
            Some("desc") if !prefixed_desc => true,
            _ => {
                return Err(BeadsError::Validation {
                    field: "sort".to_string(),
                    reason: format!("invalid direction in roadmap sort term '{raw_term}'"),
                });
            }
        };
        terms.push(SortTerm { key, descending });
    }
    if terms.is_empty() {
        return Err(BeadsError::Validation {
            field: "sort".to_string(),
            reason: "at least one roadmap sort key is required".to_string(),
        });
    }
    Ok(terms)
}

fn compare_issues(
    left: &str,
    right: &str,
    issues: &BTreeMap<String, Issue>,
    sort: &[SortTerm],
) -> Ordering {
    let Some(left_issue) = issues.get(left) else {
        return left.cmp(right);
    };
    let Some(right_issue) = issues.get(right) else {
        return left.cmp(right);
    };
    for term in sort {
        let order = match term.key {
            SortKey::Priority => left_issue.priority.0.cmp(&right_issue.priority.0),
            SortKey::Status => status_rank(left_issue.status.as_str())
                .cmp(&status_rank(right_issue.status.as_str())),
            SortKey::Title => left_issue
                .title
                .to_lowercase()
                .cmp(&right_issue.title.to_lowercase()),
            SortKey::Id => compare_ids(left, right),
        };
        let order = if term.descending {
            order.reverse()
        } else {
            order
        };
        if !order.is_eq() {
            return order;
        }
    }
    compare_ids(left, right)
}

fn compare_ids(left: &str, right: &str) -> Ordering {
    natural_parts(left).cmp(&natural_parts(right))
}

fn natural_parts(value: &str) -> Vec<NaturalPart> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut digits = None;
    for character in value.chars() {
        let is_digit = character.is_ascii_digit();
        if digits.is_some_and(|was_digit| was_digit != is_digit) {
            parts.push(NaturalPart::new(&current, digits.unwrap_or(false)));
            current.clear();
        }
        digits = Some(is_digit);
        current.push(character);
    }
    if !current.is_empty() {
        parts.push(NaturalPart::new(&current, digits.unwrap_or(false)));
    }
    parts
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd)]
enum NaturalPart {
    Text(String),
    Number(u128, usize),
}

impl NaturalPart {
    fn new(value: &str, digits: bool) -> Self {
        if digits {
            Self::Number(value.parse().unwrap_or(u128::MAX), value.len())
        } else {
            Self::Text(value.to_lowercase())
        }
    }
}

fn status_rank(status: &str) -> usize {
    match status {
        "open" => 0,
        "in_progress" => 1,
        "blocked" => 2,
        "deferred" => 3,
        "draft" => 4,
        "closed" => 5,
        "tombstone" => 6,
        "pinned" => 7,
        _ => 8,
    }
}

fn render_output(
    output: &RoadmapOutput,
    args: &RoadmapArgs,
    format: RoadmapOutputFormat,
    ctx: &OutputContext,
) {
    match format {
        RoadmapOutputFormat::Json => {
            let json_ctx = OutputContext::from_output_format(
                crate::cli::OutputFormat::Json,
                ctx.is_quiet(),
                true,
            );
            json_ctx.json_pretty(output);
        }
        RoadmapOutputFormat::Mermaid => {
            render_mermaid(output);
            render_diagnostics(output);
        }
        RoadmapOutputFormat::Dot => {
            render_dot(output);
            render_diagnostics(output);
        }
        RoadmapOutputFormat::Text => {
            let forced_plain = (args.format == Some(RoadmapOutputFormat::Text)).then(|| {
                OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true)
            });
            render_text(output, args, forced_plain.as_ref().unwrap_or(ctx));
            render_diagnostics(output);
        }
    }
}

fn render_text(output: &RoadmapOutput, args: &RoadmapArgs, ctx: &OutputContext) {
    if args.only != Some(RoadmapSection::Summary) {
        if ctx.is_rich() {
            ctx.section("Roadmap Graph");
        } else {
            ctx.print_line("Roadmap Graph");
        }
        let nodes: HashMap<&str, &RoadmapNode> = output
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), node))
            .collect();
        let forest = placement_forest(&output.placements);
        for (root_index, placement_index) in forest.roots.iter().enumerate() {
            if root_index > 0 {
                ctx.print_line("");
            }
            render_placement(
                *placement_index,
                true,
                &[],
                output,
                &nodes,
                &forest,
                args.wrap,
                ctx,
            );
        }
    }
    for (index, summary) in output.summaries.iter().enumerate() {
        if args.only != Some(RoadmapSection::Summary) || index > 0 {
            ctx.print_line("");
        }
        if ctx.is_rich() {
            render_rich_summary(summary, ctx);
        } else {
            render_plain_summary(summary, ctx);
        }
    }
}

struct PlacementForest {
    roots: Vec<usize>,
    children: Vec<Vec<usize>>,
}

fn placement_forest(placements: &[RoadmapPlacement]) -> PlacementForest {
    let mut roots = Vec::new();
    let mut children = vec![Vec::new(); placements.len()];
    let mut ancestors: Vec<usize> = Vec::new();
    for (index, placement) in placements.iter().enumerate() {
        ancestors.truncate(placement.depth);
        if placement.depth == 0 || ancestors.len() < placement.depth {
            roots.push(index);
        } else if let Some(parent) = ancestors.last() {
            children[*parent].push(index);
        }
        ancestors.push(index);
    }
    PlacementForest { roots, children }
}

#[allow(clippy::too_many_arguments)]
fn render_placement(
    index: usize,
    last: bool,
    ancestor_has_more: &[bool],
    output: &RoadmapOutput,
    nodes: &HashMap<&str, &RoadmapNode>,
    forest: &PlacementForest,
    wrap: bool,
    ctx: &OutputContext,
) {
    let placement = &output.placements[index];
    let Some(node) = nodes.get(placement.id.as_str()) else {
        return;
    };
    if ctx.is_rich() {
        render_rich_placement(node, placement, last, ancestor_has_more, wrap, ctx);
    } else {
        render_plain_placement(node, placement, last, ancestor_has_more, wrap, ctx);
    }

    let mut child_ancestors = ancestor_has_more.to_vec();
    if placement.depth > 0 {
        child_ancestors.push(!last);
    }
    for (child_offset, child_index) in forest.children[index].iter().enumerate() {
        render_placement(
            *child_index,
            child_offset + 1 == forest.children[index].len(),
            &child_ancestors,
            output,
            nodes,
            forest,
            wrap,
            ctx,
        );
    }
}

fn placement_suffix(placement: &RoadmapPlacement) -> String {
    let mut suffix = String::new();
    if let Some(relation) = placement.relation.as_deref()
        && (relation != "parent-child" || placement.hidden_count > 0)
    {
        if placement.hidden_count > 0 {
            suffix.push_str(&format!(
                " [{relation}; compressed {} hidden]",
                placement.hidden_count
            ));
        } else {
            suffix.push_str(&format!(" [{relation}]"));
        }
    }
    if placement.requested {
        suffix.push_str(" ← requested");
    }
    if placement.reference {
        suffix.push_str(" ↩ reference");
    } else if placement.truncated {
        if placement.requested_route {
            suffix.push_str(" … truncated toward requested");
        } else {
            suffix.push_str(" … truncated");
        }
    }
    suffix
}

fn guide_prefix(placement: &RoadmapPlacement, last: bool, ancestor_has_more: &[bool]) -> String {
    let mut prefix = String::new();
    for has_more in ancestor_has_more {
        prefix.push_str(if *has_more { "│  " } else { "   " });
    }
    if placement.depth > 0 {
        prefix.push_str(if last { "└─ " } else { "├─ " });
    }
    prefix
}

fn rich_guide_prefix(
    placement: &RoadmapPlacement,
    last: bool,
    ancestor_has_more: &[bool],
) -> String {
    let mut prefix = String::new();
    for has_more in ancestor_has_more {
        prefix.push_str(if *has_more { "│   " } else { "    " });
    }
    if placement.depth > 0 {
        prefix.push_str(if last { "╰── " } else { "├── " });
    }
    prefix
}

fn node_metadata(node: &RoadmapNode) -> String {
    let status = node
        .status
        .parse::<Status>()
        .unwrap_or_else(|_| Status::Custom(node.status.clone()));
    format!(
        "{} {} [{} P{}] ",
        crate::format::format_status_icon(&status),
        crate::format::sanitize_terminal_inline(&node.id),
        crate::format::sanitize_terminal_inline(&node.issue_type),
        node.priority
    )
}

fn render_plain_placement(
    node: &RoadmapNode,
    placement: &RoadmapPlacement,
    last: bool,
    ancestor_has_more: &[bool],
    wrap: bool,
    ctx: &OutputContext,
) {
    let prefix = guide_prefix(placement, last, ancestor_has_more);
    let metadata = node_metadata(node);
    let lead = format!("{prefix}{metadata}");
    let suffix = placement_suffix(placement);
    let width = crate::format::terminal_width();
    let available = width
        .saturating_sub(unicode_width::UnicodeWidthStr::width(lead.as_str()))
        .saturating_sub(unicode_width::UnicodeWidthStr::width(suffix.as_str()))
        .max(1);
    let title = crate::format::sanitize_terminal_inline(&node.title);
    if wrap {
        let lines = wrap_visible(title.as_ref(), available);
        for (line_index, line) in lines.iter().enumerate() {
            let line_suffix = if line_index + 1 == lines.len() {
                suffix.as_str()
            } else {
                ""
            };
            if line_index == 0 {
                ctx.print_line(&format!("{lead}{line}{line_suffix}"));
            } else {
                ctx.print_line(&format!(
                    "{}{line}{line_suffix}",
                    " ".repeat(unicode_width::UnicodeWidthStr::width(lead.as_str()))
                ));
            }
        }
    } else {
        ctx.print_line(&format!(
            "{lead}{}{suffix}",
            crate::format::truncate_title(title.as_ref(), available)
        ));
    }
}

fn render_rich_placement(
    node: &RoadmapNode,
    placement: &RoadmapPlacement,
    last: bool,
    ancestor_has_more: &[bool],
    wrap: bool,
    ctx: &OutputContext,
) {
    let theme = ctx.theme();
    let prefix = rich_guide_prefix(placement, last, ancestor_has_more);
    let metadata = node_metadata(node);
    let suffix = placement_suffix(placement);
    let available = crate::format::terminal_width()
        .saturating_sub(unicode_width::UnicodeWidthStr::width(prefix.as_str()))
        .saturating_sub(unicode_width::UnicodeWidthStr::width(metadata.as_str()))
        .saturating_sub(unicode_width::UnicodeWidthStr::width(suffix.as_str()))
        .max(1);
    let title = crate::format::sanitize_terminal_inline(&node.title);
    let title_lines = if wrap {
        wrap_visible(title.as_ref(), available)
    } else {
        vec![crate::format::truncate_title(title.as_ref(), available)]
    };

    for (line_index, title_line) in title_lines.iter().enumerate() {
        let mut line = Text::new("");
        if line_index == 0 {
            line.append_styled(&prefix, theme.dimmed.clone());
            let status = node
                .status
                .parse::<Status>()
                .unwrap_or_else(|_| Status::Custom(node.status.clone()));
            let issue_type = node
                .issue_type
                .parse::<IssueType>()
                .unwrap_or_else(|_| IssueType::Custom(node.issue_type.clone()));
            let priority = Priority(node.priority);
            line.append_styled(
                crate::format::format_status_icon(&status),
                theme.status_style(&status),
            );
            line.append(" ");
            line.append_styled(
                crate::format::sanitize_terminal_inline(&node.id).as_ref(),
                theme.issue_id.clone(),
            );
            line.append(" [");
            line.append_styled(
                crate::format::sanitize_terminal_inline(&node.issue_type).as_ref(),
                theme.type_style(&issue_type),
            );
            line.append(" ");
            line.append_styled(&priority.to_string(), theme.priority_style(priority));
            line.append("] ");
        } else {
            line.append(&" ".repeat(
                unicode_width::UnicodeWidthStr::width(prefix.as_str())
                    + unicode_width::UnicodeWidthStr::width(metadata.as_str()),
            ));
        }
        line.append_styled(title_line, theme.issue_title.clone());
        if line_index + 1 == title_lines.len() {
            line.append_styled(&suffix, theme.dimmed.clone());
        }
        line.append("\n");
        ctx.render(&line);
    }
}

fn render_plain_summary(summary: &RoadmapSummary, ctx: &OutputContext) {
    ctx.print_line(&format!("Summary: {} {}", summary.id, summary.title));
    ctx.print_line(summary.scope);
    ctx.print_line("Type                 Done  Total  Percent");
    for row in &summary.rows {
        ctx.print_line(&summary_row(
            &row.issue_type,
            row.terminal,
            row.count,
            row.completion_percent,
        ));
    }
    ctx.print_line("--------------------  ----  -----  -------");
    ctx.print_line(&summary_row(
        "Total",
        summary.total.terminal,
        summary.total.count,
        summary.total.completion_percent,
    ));
}

fn render_rich_summary(summary: &RoadmapSummary, ctx: &OutputContext) {
    let theme = ctx.theme();
    let mut content = Text::new("");
    content.append_styled(summary.scope, theme.dimmed.clone());
    content.append("\n");
    content.append_styled(
        "Type               Progress              Done      %\n",
        theme.emphasis.clone(),
    );
    for row in &summary.rows {
        content.append(&rich_summary_row(
            &row.issue_type,
            row.terminal,
            row.count,
            row.completion_percent,
        ));
        content.append("\n");
    }
    content.append_styled(&"─".repeat(52), theme.dimmed.clone());
    content.append("\n");
    content.append_styled(
        &rich_summary_row(
            "Total",
            summary.total.terminal,
            summary.total.count,
            summary.total.completion_percent,
        ),
        theme.emphasis.clone(),
    );
    let title = format!("{} — {}", summary.id, summary.title);
    let panel = Panel::from_rich_text(&content, crate::format::terminal_width())
        .title(Text::styled(title, theme.panel_title.clone()))
        .box_style(theme.box_style)
        .border_style(theme.panel_border.clone());
    ctx.render(&panel);
}

fn summary_row(
    label: &str,
    terminal: usize,
    count: usize,
    completion_percent: Option<usize>,
) -> String {
    let percent = completion_percent.map_or_else(|| "—".to_string(), |value| format!("{value}%"));
    format!("{label:<20} {terminal:>4}  {count:>5}  {percent:>7}")
}

fn rich_summary_row(
    label: &str,
    terminal: usize,
    count: usize,
    completion_percent: Option<usize>,
) -> String {
    let percent = completion_percent.unwrap_or(0);
    let percent_label =
        completion_percent.map_or_else(|| "—".to_string(), |value| format!("{value}%"));
    let filled = percent / 5;
    format!(
        "{label:<18} {}{} {terminal:>3}/{count:<3} {percent_label:>4}",
        "━".repeat(filled),
        "─".repeat(20usize.saturating_sub(filled)),
    )
}

fn render_diagnostics(output: &RoadmapOutput) {
    for diagnostic in &output.diagnostics {
        eprintln!("warning[{}]: {}", diagnostic.code, diagnostic.message);
    }
}

fn wrap_visible(value: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in value.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if unicode_width::UnicodeWidthStr::width(candidate.as_str()) <= width {
            current = candidate;
        } else {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            if unicode_width::UnicodeWidthStr::width(word) <= width {
                current = word.to_string();
            } else {
                let mut chunk = String::new();
                let mut chunk_width = 0;
                for character in word.chars() {
                    let character_width =
                        unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
                    if chunk_width + character_width > width && !chunk.is_empty() {
                        lines.push(std::mem::take(&mut chunk));
                        chunk_width = 0;
                    }
                    chunk.push(character);
                    chunk_width += character_width;
                }
                current = chunk;
            }
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

fn render_mermaid(output: &RoadmapOutput) {
    println!("flowchart TD");
    let ids: BTreeMap<&str, String> = output
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.as_str(), format!("n{index}")))
        .collect();
    for node in &output.nodes {
        println!(
            "  {}[\"{}\"]",
            ids[node.id.as_str()],
            escape_mermaid(&format!("{}: {}", node.id, node.title))
        );
    }
    for edge in &output.edges {
        if let (Some(from), Some(to)) = (ids.get(edge.from.as_str()), ids.get(edge.to.as_str())) {
            let arrow = match edge.relation.as_str() {
                "derived-from" => "-.->",
                "implements" => "==>",
                _ => "-->",
            };
            println!("  {from} {arrow}|{}| {to}", escape_mermaid(&edge.relation));
        }
    }
}

fn render_dot(output: &RoadmapOutput) {
    println!("digraph roadmap {{");
    for node in &output.nodes {
        println!(
            "  \"{}\" [label=\"{}\"];",
            escape_dot(&node.id),
            escape_dot(&format!("{}: {}", node.id, node.title))
        );
    }
    for edge in &output.edges {
        let style = match edge.relation.as_str() {
            "derived-from" => "dashed",
            "implements" => "bold",
            _ => "solid",
        };
        println!(
            "  \"{}\" -> \"{}\" [label=\"{}\", style=\"{style}\"];",
            escape_dot(&edge.from),
            escape_dot(&edge.to),
            escape_dot(&edge.relation)
        );
    }
    println!("}}");
}

fn escape_mermaid(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\n', " ")
}

fn escape_dot(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::{compare_ids, escape_dot, escape_mermaid, parse_sort, percent};

    #[test]
    fn diagram_escaping_is_deterministic() {
        assert_eq!(escape_mermaid("a\"<&\nb"), "a&quot;&lt;&amp; b");
        assert_eq!(escape_dot("a\\\"\nb"), "a\\\\\\\"\\nb");
    }

    #[test]
    fn summary_percent_uses_nearest_integer() {
        assert_eq!(percent(2, 3), Some(67));
        assert_eq!(percent(0, 0), None);
    }

    #[test]
    fn sort_parser_rejects_unknown_and_duplicate_keys() {
        assert!(parse_sort("priority,title").is_ok());
        assert!(parse_sort("-priority,+title").is_ok());
        assert!(parse_sort("priority,priority").is_err());
        assert!(parse_sort("created_at").is_err());
        assert!(parse_sort("-priority:desc").is_err());
        assert!(compare_ids("br-2", "br-10").is_lt());
    }
}
