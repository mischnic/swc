//! Encoding of the ECMA-426 source map `scopes` proposal
//! (<https://github.com/tc39/ecma426/blob/main/proposals/scopes.md>).
//!
//! The scope records are collected by the code generator (see
//! [`swc_ecma_codegen::text_writer::JsWriter::new_with_scopes`]). Typical
//! usage:
//!
//! 1. Emit the program with a
//!    [`JsWriter`](swc_ecma_codegen::text_writer::JsWriter) created via
//!    `new_with_scopes`.
//! 2. Register [`collect_additional_names`] in the `names` of the source map
//!    (e.g. via [`swc_common::source_map::SourceMapGenConfig::for_each_additional_name`])
//!    while building it.
//! 3. Call [`encode_scopes`] and store the result with
//!    [`swc_sourcemap::SourceMap::set_scopes`].

use rustc_hash::{FxHashMap, FxHashSet};
use swc_common::{FileName, SourceMapper, Span};
use swc_ecma_codegen::text_writer::ScopeRecord;

#[derive(Debug, Clone, Copy)]
struct Pos {
    line: u32,
    column: u32,
}

#[derive(Debug, Clone)]
struct OriginalScopeNode {
    source_idx: u32,
    start: Pos,
    end: Pos,
    kind: Option<String>,
    name: Option<String>,
    is_stack_frame: bool,
    variables: Vec<String>,
    children: Vec<usize>,
}

#[derive(Debug, Clone)]
struct GeneratedRangeNode {
    start: Pos,
    end: Pos,
    is_stack_frame: bool,
    is_hidden: bool,
    original_scope: Option<usize>,
    bindings: Vec<Option<String>>,
    children: Vec<usize>,
}

#[derive(Debug, Clone, Copy)]
struct OriginalPosState {
    prev_line: u32,
    prev_column: u32,
}

#[derive(Debug, Clone, Copy)]
struct OriginalNameState {
    prev_name: i64,
    prev_kind: i64,
    prev_variable: i64,
}

#[derive(Debug, Clone, Copy)]
struct GeneratedState {
    prev_line: u32,
    prev_column: u32,
    prev_definition: i64,
}

/// Returns the strings that [`encode_scopes`] references from the `names` of
/// the source map (scope kinds, scope names, variable names and binding
/// expressions), in a stable order and without duplicates.
///
/// These have to be added to the source map's `names` before calling
/// [`encode_scopes`], otherwise encoding fails.
pub fn collect_additional_names(scopes: &[ScopeRecord]) -> Vec<String> {
    let mut names = vec![];
    let mut seen = FxHashSet::default();

    for scope in scopes {
        let kind = scope.kind.as_str().to_string();
        if seen.insert(kind.clone()) {
            names.push(kind);
        }

        if let Some(name) = &scope.name {
            if seen.insert(name.clone()) {
                names.push(name.clone());
            }
        }

        for binding in &scope.bindings {
            if seen.insert(binding.name.clone()) {
                names.push(binding.name.clone());
            }

            if let Some(expr) = &binding.expression {
                if seen.insert(expr.clone()) {
                    names.push(expr.clone());
                }
            }
        }
    }

    names
}

/// Encodes `scopes` as the value of the source map `scopes` field.
///
/// - `cm` is used to resolve the original spans of the scopes, e.g. the
///   [`swc_common::SourceMap`] used for parsing.
/// - `map` is the source map built for the generated code. Its `sources` and
///   `names` are used to resolve indices, so it must contain the names returned
///   by [`collect_additional_names`].
/// - `source_name_for_file` must map a file name to the corresponding entry in
///   the `sources` of `map` (i.e. the same mapping used when building `map`).
///
/// Original scopes are emitted in original position order. Original scopes
/// that can't be represented as a properly nested tree (e.g. because a
/// transform produced overlapping spans) are omitted; generated ranges that
/// referenced them are kept without a definition.
///
/// Returns `None` if there is nothing to encode or a referenced name is
/// missing from `map`.
pub fn encode_scopes(
    scopes: &[ScopeRecord],
    cm: &dyn SourceMapper,
    map: &swc_sourcemap::SourceMap,
    mut source_name_for_file: impl FnMut(&FileName) -> String,
) -> Option<String> {
    if scopes.is_empty() {
        return None;
    }

    let source_count = map.get_source_count() as usize;
    if source_count == 0 {
        return None;
    }

    let mut source_name_to_idx = FxHashMap::default();
    for idx in 0..map.get_source_count() {
        let source = map.get_source(idx)?.to_string();
        source_name_to_idx.insert(source, idx);
    }

    let mut name_to_idx = FxHashMap::default();
    for (idx, name) in map.names().enumerate() {
        name_to_idx.insert(name.to_string(), idx as u32);
    }

    let mut generated_children = vec![Vec::new(); scopes.len()];
    let mut generated_roots = vec![];
    for (idx, scope) in scopes.iter().enumerate() {
        let parent = scope
            .parent
            .and_then(|p| usize::try_from(p).ok())
            .filter(|parent| *parent < scopes.len());
        if let Some(parent) = parent {
            generated_children[parent].push(idx);
        } else {
            generated_roots.push(idx);
        }
    }

    let mut original_nodes: Vec<OriginalScopeNode> = vec![];
    let mut original_roots_by_source: Vec<Vec<usize>> = vec![vec![]; source_count];
    let mut scope_to_original: Vec<Option<usize>> = vec![None; scopes.len()];

    for (scope_idx, scope) in scopes.iter().enumerate() {
        let Some((source_idx, start, end)) = scope
            .original_span
            .and_then(|span| span_to_original_position(cm, span, &mut source_name_for_file))
            .and_then(|(source_name, start, end)| {
                source_name_to_idx
                    .get(&source_name)
                    .copied()
                    .map(|source_idx| (source_idx, start, end))
            })
        else {
            continue;
        };

        let mut parent = scope
            .parent
            .and_then(|p| usize::try_from(p).ok())
            .filter(|parent| *parent < scopes.len());
        let mut parent_original = None;
        while let Some(parent_scope_idx) = parent {
            if let Some(parent_original_idx) = scope_to_original[parent_scope_idx] {
                if original_nodes[parent_original_idx].source_idx == source_idx {
                    parent_original = Some(parent_original_idx);
                    break;
                }
            }
            parent = scopes[parent_scope_idx]
                .parent
                .and_then(|p| usize::try_from(p).ok())
                .filter(|next_parent| *next_parent < scopes.len());
        }

        let original_idx = original_nodes.len();
        original_nodes.push(OriginalScopeNode {
            source_idx,
            start,
            end,
            kind: Some(scope.kind.as_str().to_string()),
            name: scope.name.clone(),
            is_stack_frame: scope.is_stack_frame,
            variables: scope
                .bindings
                .iter()
                .map(|binding| binding.name.clone())
                .collect(),
            children: vec![],
        });
        scope_to_original[scope_idx] = Some(original_idx);

        if let Some(parent_original_idx) = parent_original {
            original_nodes[parent_original_idx]
                .children
                .push(original_idx);
        } else {
            original_roots_by_source[source_idx as usize].push(original_idx);
        }
    }

    for roots in &mut original_roots_by_source {
        if roots.len() <= 1 {
            continue;
        }

        let mut min_start = original_nodes[roots[0]].start;
        let mut max_end = original_nodes[roots[0]].end;
        for &root in roots.iter() {
            let start = original_nodes[root].start;
            if (start.line, start.column) < (min_start.line, min_start.column) {
                min_start = start;
            }

            let end = original_nodes[root].end;
            if (end.line, end.column) > (max_end.line, max_end.column) {
                max_end = end;
            }
        }

        let source_idx = original_nodes[roots[0]].source_idx;
        let synthetic_root = original_nodes.len();
        original_nodes.push(OriginalScopeNode {
            source_idx,
            start: min_start,
            end: max_end,
            kind: Some("module".to_string()),
            name: None,
            is_stack_frame: false,
            variables: vec![],
            children: std::mem::take(roots),
        });
        roots.push(synthetic_root);
    }

    // Original scopes were collected in *generated* order. Transforms (e.g. the
    // compressor or a bundler) may reorder code, so put them into original
    // position order and drop scopes that can't be represented as a properly
    // nested tree. Otherwise the relative (unsigned) position encoding below
    // would produce corrupted positions.
    let mut removed_original = vec![false; original_nodes.len()];
    for roots in &mut original_roots_by_source {
        if let Some(&root) = roots.first() {
            normalize_original_scope_tree(root, &mut original_nodes, &mut removed_original);
            if removed_original[root] {
                roots.clear();
            }
        }
    }
    for original in &mut scope_to_original {
        if original.is_some_and(|idx| removed_original[idx]) {
            *original = None;
        }
    }

    let mut generated_nodes = vec![];
    let mut generated_top_level = vec![];
    for &root in &generated_roots {
        let generated_root = build_generated_range_tree(
            root,
            scopes,
            &generated_children,
            &scope_to_original,
            &original_nodes,
            &mut generated_nodes,
        );
        generated_top_level.push(generated_root);
    }

    let mut definition_indices = FxHashMap::default();
    let mut next_definition = 0usize;
    let mut has_original_scopes = false;
    for roots in &original_roots_by_source {
        if roots.is_empty() {
            continue;
        }
        has_original_scopes = true;
        let root = roots[0];
        assign_definition_indices(
            root,
            &original_nodes,
            &mut definition_indices,
            &mut next_definition,
        );
    }

    for range in &generated_nodes {
        if let Some(original_scope) = range.original_scope {
            if !definition_indices.contains_key(&original_scope) {
                return None;
            }
        }
    }

    let mut items = vec![];

    if has_original_scopes {
        let mut original_name_state = OriginalNameState {
            prev_name: 0,
            prev_kind: 0,
            prev_variable: 0,
        };

        let mut source_items = vec![];
        for roots in &original_roots_by_source {
            if roots.is_empty() {
                source_items.push(String::new());
                continue;
            }

            let mut tree_items = vec![];
            let mut pos_state = OriginalPosState {
                prev_line: 0,
                prev_column: 0,
            };
            encode_original_scope(
                roots[0],
                &original_nodes,
                &name_to_idx,
                &mut original_name_state,
                &mut pos_state,
                &mut tree_items,
            )?;
            source_items.push(tree_items.join(","));
        }

        items.push(source_items.join(","));
    }

    if !generated_top_level.is_empty() {
        let mut generated_items = vec![];
        let mut generated_state = GeneratedState {
            prev_line: 0,
            prev_column: 0,
            prev_definition: 0,
        };

        for &root in &generated_top_level {
            encode_generated_range(
                root,
                &generated_nodes,
                &name_to_idx,
                &definition_indices,
                &mut generated_state,
                &mut generated_items,
            )?;
        }

        if !generated_items.is_empty() {
            items.push(generated_items.join(","));
        }
    }

    let scopes = items.join(",");
    if scopes.is_empty() {
        None
    } else {
        Some(scopes)
    }
}

fn build_generated_range_tree(
    scope_idx: usize,
    scopes: &[ScopeRecord],
    children_by_scope: &[Vec<usize>],
    scope_to_original: &[Option<usize>],
    original_nodes: &[OriginalScopeNode],
    generated_nodes: &mut Vec<GeneratedRangeNode>,
) -> usize {
    let scope = &scopes[scope_idx];
    let original_scope = scope_to_original[scope_idx];
    let bindings = if let Some(original_scope_idx) = original_scope {
        original_nodes[original_scope_idx]
            .variables
            .iter()
            .map(|name| {
                scope
                    .bindings
                    .iter()
                    .find(|binding| &binding.name == name)
                    .and_then(|binding| binding.expression.clone())
            })
            .collect()
    } else {
        vec![]
    };

    let generated_idx = generated_nodes.len();
    generated_nodes.push(GeneratedRangeNode {
        start: Pos {
            line: scope.generated_start.line,
            column: scope.generated_start.col,
        },
        end: scope
            .generated_end
            .map(|end| Pos {
                line: end.line,
                column: end.col,
            })
            .unwrap_or(Pos {
                line: scope.generated_start.line,
                column: scope.generated_start.col,
            }),
        is_stack_frame: scope.is_stack_frame,
        is_hidden: scope.is_hidden,
        original_scope,
        bindings,
        children: vec![],
    });

    for &child_scope_idx in &children_by_scope[scope_idx] {
        let child_generated_idx = build_generated_range_tree(
            child_scope_idx,
            scopes,
            children_by_scope,
            scope_to_original,
            original_nodes,
            generated_nodes,
        );
        generated_nodes[generated_idx]
            .children
            .push(child_generated_idx);
    }

    generated_idx
}

fn span_to_original_position(
    cm: &dyn SourceMapper,
    span: Span,
    source_name_for_file: &mut impl FnMut(&FileName) -> String,
) -> Option<(String, Pos, Pos)> {
    if span.lo().is_dummy() || span.hi().is_dummy() {
        return None;
    }

    let start = cm.lookup_char_pos(span.lo());
    let end = cm.lookup_char_pos(span.hi());
    if start.line == 0 || end.line == 0 {
        return None;
    }

    let source_name = source_name_for_file(&start.file.name);
    Some((
        source_name,
        Pos {
            line: start.line as u32 - 1,
            column: start.col.0 as u32,
        },
        Pos {
            line: end.line as u32 - 1,
            column: end.col.0 as u32,
        },
    ))
}

#[inline]
fn pos_key(pos: Pos) -> (u32, u32) {
    (pos.line, pos.column)
}

/// Sorts the children of every original scope by their original start
/// position and removes children (with their whole subtree) that are not
/// contained in their parent or overlap a preceding sibling.
///
/// The scopes proposal requires original scopes to form a properly nested
/// tree with siblings in source order, as positions are encoded relative to
/// the preceding item with unsigned deltas.
///
/// The root itself is removed if its start is after its end.
fn normalize_original_scope_tree(
    root: usize,
    nodes: &mut [OriginalScopeNode],
    removed: &mut [bool],
) {
    if pos_key(nodes[root].start) > pos_key(nodes[root].end) {
        mark_original_subtree_removed(root, nodes, removed);
        return;
    }

    let mut stack = vec![root];
    while let Some(node_idx) = stack.pop() {
        let mut children = std::mem::take(&mut nodes[node_idx].children);
        // Stable sort keeps the generated order for scopes starting at the same
        // position; wider scopes first, so an outer scope stays before an inner
        // scope that starts at the same position.
        children.sort_by(|&a, &b| {
            pos_key(nodes[a].start)
                .cmp(&pos_key(nodes[b].start))
                .then_with(|| pos_key(nodes[b].end).cmp(&pos_key(nodes[a].end)))
        });

        let parent_end = pos_key(nodes[node_idx].end);
        let mut prev_end = pos_key(nodes[node_idx].start);
        let mut kept = Vec::with_capacity(children.len());
        for child in children {
            let start = pos_key(nodes[child].start);
            let end = pos_key(nodes[child].end);
            if start < prev_end || end > parent_end || start > end {
                mark_original_subtree_removed(child, nodes, removed);
                continue;
            }
            prev_end = end;
            kept.push(child);
        }

        stack.extend(kept.iter().copied());
        nodes[node_idx].children = kept;
    }
}

fn mark_original_subtree_removed(root: usize, nodes: &[OriginalScopeNode], removed: &mut [bool]) {
    let mut stack = vec![root];
    while let Some(idx) = stack.pop() {
        removed[idx] = true;
        stack.extend(nodes[idx].children.iter().copied());
    }
}

fn assign_definition_indices(
    node_idx: usize,
    nodes: &[OriginalScopeNode],
    definition_indices: &mut FxHashMap<usize, usize>,
    next_definition: &mut usize,
) {
    definition_indices.insert(node_idx, *next_definition);
    *next_definition += 1;
    for &child in &nodes[node_idx].children {
        assign_definition_indices(child, nodes, definition_indices, next_definition);
    }
}

fn encode_original_scope(
    node_idx: usize,
    nodes: &[OriginalScopeNode],
    name_to_idx: &FxHashMap<String, u32>,
    name_state: &mut OriginalNameState,
    pos_state: &mut OriginalPosState,
    out: &mut Vec<String>,
) -> Option<()> {
    let node = &nodes[node_idx];

    let name_idx = node
        .name
        .as_ref()
        .and_then(|name| name_to_idx.get(name).copied());
    let kind_idx = node
        .kind
        .as_ref()
        .and_then(|kind| name_to_idx.get(kind).copied());

    let line_delta = node.start.line.saturating_sub(pos_state.prev_line);
    let column = if line_delta == 0 {
        node.start.column.saturating_sub(pos_state.prev_column)
    } else {
        node.start.column
    };

    let mut flags = 0u32;
    if name_idx.is_some() {
        flags |= 0x1;
    }
    if kind_idx.is_some() {
        flags |= 0x2;
    }
    if node.is_stack_frame {
        flags |= 0x4;
    }

    let mut start_item = String::new();
    start_item.push_str(&encode_u(1));
    start_item.push_str(&encode_u(flags as u64));
    start_item.push_str(&encode_u(line_delta as u64));
    start_item.push_str(&encode_u(column as u64));
    if let Some(name_idx) = name_idx {
        let delta = i64::from(name_idx) - name_state.prev_name;
        start_item.push_str(&encode_s(delta));
        name_state.prev_name = i64::from(name_idx);
    }
    if let Some(kind_idx) = kind_idx {
        let delta = i64::from(kind_idx) - name_state.prev_kind;
        start_item.push_str(&encode_s(delta));
        name_state.prev_kind = i64::from(kind_idx);
    }
    out.push(start_item);

    pos_state.prev_line = node.start.line;
    pos_state.prev_column = node.start.column;

    if !node.variables.is_empty() {
        let mut variable_item = String::new();
        variable_item.push_str(&encode_u(3));
        for variable in &node.variables {
            let idx = i64::from(*name_to_idx.get(variable)?);
            let delta = idx - name_state.prev_variable;
            variable_item.push_str(&encode_s(delta));
            name_state.prev_variable = idx;
        }
        out.push(variable_item);
    }

    for &child in &node.children {
        encode_original_scope(child, nodes, name_to_idx, name_state, pos_state, out)?;
    }

    let end_line_delta = node.end.line.saturating_sub(pos_state.prev_line);
    let end_column = if end_line_delta == 0 {
        node.end.column.saturating_sub(pos_state.prev_column)
    } else {
        node.end.column
    };
    let mut end_item = String::new();
    end_item.push_str(&encode_u(2));
    end_item.push_str(&encode_u(end_line_delta as u64));
    end_item.push_str(&encode_u(end_column as u64));
    out.push(end_item);

    pos_state.prev_line = node.end.line;
    pos_state.prev_column = node.end.column;

    Some(())
}

fn encode_generated_range(
    node_idx: usize,
    nodes: &[GeneratedRangeNode],
    name_to_idx: &FxHashMap<String, u32>,
    definition_indices: &FxHashMap<usize, usize>,
    state: &mut GeneratedState,
    out: &mut Vec<String>,
) -> Option<()> {
    let node = &nodes[node_idx];

    let line_delta = node.start.line.saturating_sub(state.prev_line);
    let has_line = line_delta != 0;
    let mut flags = 0u32;
    if has_line {
        flags |= 0x1;
    }
    if node.original_scope.is_some() {
        flags |= 0x2;
    }
    if node.is_stack_frame {
        flags |= 0x4;
    }
    if node.is_hidden {
        flags |= 0x8;
    }

    let mut start_item = String::new();
    start_item.push_str(&encode_u(4));
    start_item.push_str(&encode_u(flags as u64));
    if has_line {
        start_item.push_str(&encode_u(line_delta as u64));
        start_item.push_str(&encode_u(node.start.column as u64));
    } else {
        let delta = node.start.column.saturating_sub(state.prev_column);
        start_item.push_str(&encode_u(delta as u64));
    }

    if let Some(original_scope) = node.original_scope {
        let def = *definition_indices.get(&original_scope)? as i64;
        let delta = def - state.prev_definition;
        start_item.push_str(&encode_s(delta));
        state.prev_definition = def;
    }
    out.push(start_item);

    state.prev_line = node.start.line;
    state.prev_column = node.start.column;

    if !node.bindings.is_empty() {
        let mut binding_item = String::new();
        binding_item.push_str(&encode_u(6));
        for binding in &node.bindings {
            let binding_idx = match binding {
                Some(binding) => u64::from(*name_to_idx.get(binding)?) + 1,
                None => 0,
            };
            binding_item.push_str(&encode_u(binding_idx));
        }
        out.push(binding_item);
    }

    for &child in &node.children {
        encode_generated_range(child, nodes, name_to_idx, definition_indices, state, out)?;
    }

    let end_line_delta = node.end.line.saturating_sub(state.prev_line);
    let mut end_item = String::new();
    end_item.push_str(&encode_u(5));
    if end_line_delta == 0 {
        let delta = node.end.column.saturating_sub(state.prev_column);
        end_item.push_str(&encode_u(delta as u64));
    } else {
        end_item.push_str(&encode_u(end_line_delta as u64));
        end_item.push_str(&encode_u(node.end.column as u64));
    }
    out.push(end_item);

    state.prev_line = node.end.line;
    state.prev_column = node.end.column;

    Some(())
}

fn encode_u(mut value: u64) -> String {
    const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    loop {
        let mut digit = (value & 0b1_1111) as u8;
        value >>= 5;
        if value != 0 {
            digit |= 0b10_0000;
        }
        out.push(BASE64[digit as usize] as char);
        if value == 0 {
            break;
        }
    }
    out
}

fn encode_s(value: i64) -> String {
    let encoded = ((value.unsigned_abs()) << 1) | u64::from(value < 0);
    encode_u(encoded)
}

#[cfg(test)]
mod tests {
    use swc_common::{BytePos, FileName, FilePathMapping, LineCol, SourceMap, Span, DUMMY_SP};
    use swc_ecma_codegen::text_writer::{
        BindingStorage, ScopeBindingRecord, ScopeKind, ScopeRecord,
    };

    use super::{collect_additional_names, encode_scopes};

    fn make_scope(
        parent: Option<u32>,
        kind: ScopeKind,
        name: Option<&str>,
        span: Span,
        start_col: u32,
        end_col: u32,
        bindings: &[(&str, Option<&str>, BindingStorage)],
    ) -> ScopeRecord {
        ScopeRecord {
            parent,
            generated_start: LineCol {
                line: 0,
                col: start_col,
            },
            generated_end: Some(LineCol {
                line: 0,
                col: end_col,
            }),
            original_span: Some(span),
            kind,
            is_stack_frame: kind == ScopeKind::Function,
            is_hidden: false,
            name: name.map(str::to_string),
            bindings: bindings
                .iter()
                .map(|(name, expr, storage)| ScopeBindingRecord {
                    name: (*name).to_string(),
                    expression: expr.map(str::to_string),
                    storage: *storage,
                })
                .collect(),
        }
    }

    #[test]
    fn encodes_scopes_with_bindings() {
        let cm = SourceMap::new(FilePathMapping::empty());
        let fm = cm.new_source_file(
            FileName::Real("input.js".into()).into(),
            "function foo(a){let b=a;}",
        );

        let root_span = Span::new(fm.start_pos, fm.end_pos);
        let fn_span = Span::new(fm.start_pos, fm.end_pos);
        let block_span = Span::new(fm.start_pos + BytePos(15), fm.end_pos - BytePos(1));

        let scopes = vec![
            make_scope(
                None,
                ScopeKind::Global,
                Some("global"),
                root_span,
                0,
                24,
                &[("foo", Some("foo"), BindingStorage::Lexical)],
            ),
            make_scope(
                Some(0),
                ScopeKind::Function,
                Some("foo"),
                fn_span,
                0,
                24,
                &[
                    ("a", Some("a"), BindingStorage::Lexical),
                    ("b", Some("b"), BindingStorage::Lexical),
                ],
            ),
            make_scope(Some(1), ScopeKind::Block, None, block_span, 15, 23, &[]),
        ];

        let map = swc_sourcemap::SourceMap::from_slice(
            br#"{
                "version": 3,
                "sources": ["input.js"],
                "names": ["global", "function", "block", "foo", "a", "b"],
                "mappings": "AAAA"
            }"#,
        )
        .unwrap();

        let encoded = encode_scopes(&scopes, &cm, &map, |file_name| file_name.to_string())
            .expect("should encode");
        assert!(!encoded.is_empty());
        assert!(encoded.contains('B'));
        assert!(encoded.contains('E'));
        assert!(encoded.contains('G'));
    }

    #[test]
    fn fails_if_names_are_missing() {
        let cm = SourceMap::new(FilePathMapping::empty());
        let fm = cm.new_source_file(FileName::Real("input.js".into()).into(), "let x = 1;");
        let scope = make_scope(
            None,
            ScopeKind::Global,
            Some("global"),
            Span::new(fm.start_pos, fm.end_pos),
            0,
            10,
            &[("x", Some("x"), BindingStorage::Lexical)],
        );
        let map = swc_sourcemap::SourceMap::from_slice(
            br#"{
                "version": 3,
                "sources": ["input.js"],
                "names": [],
                "mappings": "AAAA"
            }"#,
        )
        .unwrap();

        assert!(encode_scopes(&[scope], &cm, &map, |file_name| file_name.to_string()).is_none());
    }

    #[test]
    fn collects_scope_related_names() {
        let scope = ScopeRecord {
            parent: None,
            generated_start: LineCol { line: 0, col: 0 },
            generated_end: Some(LineCol { line: 0, col: 1 }),
            original_span: Some(DUMMY_SP),
            kind: ScopeKind::Function,
            is_stack_frame: true,
            is_hidden: false,
            name: Some("foo".to_string()),
            bindings: vec![
                ScopeBindingRecord {
                    name: "a".to_string(),
                    expression: Some("a".to_string()),
                    storage: BindingStorage::Lexical,
                },
                ScopeBindingRecord {
                    name: "a".to_string(),
                    expression: Some("a".to_string()),
                    storage: BindingStorage::Lexical,
                },
            ],
        };

        let names = collect_additional_names(&[scope]);
        assert!(names.contains(&"function".to_string()));
        assert!(names.contains(&"foo".to_string()));
        assert!(names.contains(&"a".to_string()));
    }

    #[test]
    fn emits_generated_ranges_without_original_scope_definition() {
        let cm = SourceMap::new(FilePathMapping::empty());
        cm.new_source_file(FileName::Real("input.js".into()).into(), "let x = 1;");

        let scope = ScopeRecord {
            parent: None,
            generated_start: LineCol { line: 0, col: 0 },
            generated_end: Some(LineCol { line: 0, col: 10 }),
            original_span: Some(DUMMY_SP),
            kind: ScopeKind::Global,
            is_stack_frame: false,
            is_hidden: false,
            name: None,
            bindings: vec![ScopeBindingRecord {
                name: "x".to_string(),
                expression: Some("x".to_string()),
                storage: BindingStorage::Lexical,
            }],
        };

        let map = swc_sourcemap::SourceMap::from_slice(
            br#"{
                "version": 3,
                "sources": ["input.js"],
                "names": [],
                "mappings": "AAAA"
            }"#,
        )
        .unwrap();

        let encoded =
            encode_scopes(&[scope], &cm, &map, |file_name| file_name.to_string()).unwrap();
        assert!(encoded.starts_with('E'));
        assert!(!encoded.contains('B'));
        assert!(!encoded.contains('G'));
    }

    #[test]
    fn keeps_empty_original_scope_slot_for_source_without_scopes() {
        let cm = SourceMap::new(FilePathMapping::empty());
        let fm = cm.new_source_file(FileName::Real("a.js".into()).into(), "let a = 1;");
        let scope = make_scope(
            None,
            ScopeKind::Global,
            None,
            Span::new(fm.start_pos, fm.end_pos),
            0,
            10,
            &[],
        );

        let map = swc_sourcemap::SourceMap::from_slice(
            br#"{
                "version": 3,
                "sources": ["a.js", "b.js"],
                "names": ["global"],
                "mappings": "AAAA"
            }"#,
        )
        .unwrap();

        let encoded =
            encode_scopes(&[scope], &cm, &map, |file_name| file_name.to_string()).unwrap();
        assert!(encoded.contains(",,E"));
    }

    #[test]
    fn encoding_is_deterministic_for_same_input() {
        let cm = SourceMap::new(FilePathMapping::empty());
        let fm = cm.new_source_file(
            FileName::Real("input.js".into()).into(),
            "function foo(a){return a;}",
        );

        let scopes = vec![
            make_scope(
                None,
                ScopeKind::Global,
                Some("global"),
                Span::new(fm.start_pos, fm.end_pos),
                0,
                24,
                &[("foo", Some("foo"), BindingStorage::Lexical)],
            ),
            make_scope(
                Some(0),
                ScopeKind::Function,
                Some("foo"),
                Span::new(fm.start_pos, fm.end_pos),
                0,
                24,
                &[("a", Some("a"), BindingStorage::Lexical)],
            ),
        ];

        let map = swc_sourcemap::SourceMap::from_slice(
            br#"{
                "version": 3,
                "sources": ["input.js"],
                "names": ["global", "function", "foo", "a"],
                "mappings": "AAAA"
            }"#,
        )
        .unwrap();

        let first = encode_scopes(&scopes, &cm, &map, |file_name| file_name.to_string()).unwrap();
        let second = encode_scopes(&scopes, &cm, &map, |file_name| file_name.to_string()).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn drops_overlapping_original_scopes() {
        let cm = SourceMap::new(FilePathMapping::empty());
        let fm = cm.new_source_file(
            FileName::Real("input.js".into()).into(),
            "function a(){}function b(){}",
        );

        let root = Span::new(fm.start_pos, fm.end_pos);
        let first = Span::new(fm.start_pos, fm.start_pos + BytePos(16));
        // Overlaps `first`, e.g. because a transform produced an inconsistent span.
        let overlapping = Span::new(fm.start_pos + BytePos(10), fm.end_pos);

        let scopes = vec![
            make_scope(None, ScopeKind::Module, None, root, 0, 28, &[]),
            make_scope(Some(0), ScopeKind::Function, Some("a"), first, 0, 14, &[]),
            make_scope(
                Some(0),
                ScopeKind::Function,
                Some("b"),
                overlapping,
                14,
                28,
                &[],
            ),
        ];

        let map = swc_sourcemap::SourceMap::from_slice(
            br#"{
                "version": 3,
                "sources": ["input.js"],
                "names": ["module", "function", "a", "b"],
                "mappings": "AAAA"
            }"#,
        )
        .unwrap();

        let encoded = encode_scopes(&scopes, &cm, &map, |file_name| file_name.to_string())
            .expect("should encode");
        let items = encoded.split(',').collect::<Vec<_>>();
        // Original scopes: the module and `a` only (start and end items each).
        assert_eq!(items.iter().filter(|i| i.starts_with('B')).count(), 2);
        assert_eq!(items.iter().filter(|i| i.starts_with('C')).count(), 2);
        // All three generated ranges are kept, the one for `b` without a definition
        // (flags: 0x4 stack frame only).
        assert_eq!(items.iter().filter(|i| i.starts_with('E')).count(), 3);
        assert!(items.contains(&"EEA"), "{encoded}");
    }

    #[test]
    fn collects_additional_names_in_stable_order() {
        let scope = ScopeRecord {
            parent: None,
            generated_start: LineCol { line: 0, col: 0 },
            generated_end: Some(LineCol { line: 0, col: 1 }),
            original_span: Some(DUMMY_SP),
            kind: ScopeKind::Function,
            is_stack_frame: true,
            is_hidden: false,
            name: Some("fn".to_string()),
            bindings: vec![
                ScopeBindingRecord {
                    name: "b".to_string(),
                    expression: Some("expr".to_string()),
                    storage: BindingStorage::Lexical,
                },
                ScopeBindingRecord {
                    name: "a".to_string(),
                    expression: Some("expr".to_string()),
                    storage: BindingStorage::Lexical,
                },
            ],
        };

        let names = collect_additional_names(&[scope]);
        assert_eq!(
            names,
            vec![
                "function".to_string(),
                "fn".to_string(),
                "b".to_string(),
                "expr".to_string(),
                "a".to_string(),
            ]
        );
    }
}
