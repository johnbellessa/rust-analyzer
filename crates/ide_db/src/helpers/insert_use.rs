//! Handle syntactic aspects of inserting a new `use`.
use std::cmp::Ordering;

use hir::Semantics;
use syntax::{
    algo,
    ast::{self, make, AstNode, PathSegmentKind},
    ted, AstToken, Direction, NodeOrToken, SyntaxNode, SyntaxToken,
};

use crate::{
    helpers::merge_imports::{try_merge_imports, use_tree_path_cmp, MergeBehavior},
    RootDatabase,
};

pub use hir::PrefixKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InsertUseConfig {
    pub merge: Option<MergeBehavior>,
    pub prefix_kind: PrefixKind,
    pub group: bool,
}

#[derive(Debug, Clone)]
pub enum ImportScope {
    File(ast::SourceFile),
    Module(ast::ItemList),
}

impl ImportScope {
    pub fn from(syntax: SyntaxNode) -> Option<Self> {
        if let Some(module) = ast::Module::cast(syntax.clone()) {
            module.item_list().map(ImportScope::Module)
        } else if let this @ Some(_) = ast::SourceFile::cast(syntax.clone()) {
            this.map(ImportScope::File)
        } else {
            ast::ItemList::cast(syntax).map(ImportScope::Module)
        }
    }

    /// Determines the containing syntax node in which to insert a `use` statement affecting `position`.
    pub fn find_insert_use_container_with_macros(
        position: &SyntaxNode,
        sema: &Semantics<'_, RootDatabase>,
    ) -> Option<Self> {
        sema.ancestors_with_macros(position.clone()).find_map(Self::from)
    }

    /// Determines the containing syntax node in which to insert a `use` statement affecting `position`.
    pub fn find_insert_use_container(position: &SyntaxNode) -> Option<Self> {
        std::iter::successors(Some(position.clone()), SyntaxNode::parent).find_map(Self::from)
    }

    pub fn as_syntax_node(&self) -> &SyntaxNode {
        match self {
            ImportScope::File(file) => file.syntax(),
            ImportScope::Module(item_list) => item_list.syntax(),
        }
    }

    pub fn clone_for_update(&self) -> Self {
        match self {
            ImportScope::File(file) => ImportScope::File(file.clone_for_update()),
            ImportScope::Module(item_list) => ImportScope::Module(item_list.clone_for_update()),
        }
    }
}

/// Insert an import path into the given file/node. A `merge` value of none indicates that no import merging is allowed to occur.
pub fn insert_use<'a>(scope: &ImportScope, path: ast::Path, cfg: InsertUseConfig) {
    let _p = profile::span("insert_use");
    let use_item =
        make::use_(None, make::use_tree(path.clone(), None, None, false)).clone_for_update();
    // merge into existing imports if possible
    if let Some(mb) = cfg.merge {
        for existing_use in scope.as_syntax_node().children().filter_map(ast::Use::cast) {
            if let Some(merged) = try_merge_imports(&existing_use, &use_item, mb) {
                ted::replace(existing_use.syntax(), merged.syntax());
                return;
            }
        }
    }

    // either we weren't allowed to merge or there is no import that fits the merge conditions
    // so look for the place we have to insert to
    insert_use_(scope, path, cfg.group, use_item);
}

#[derive(Eq, PartialEq, PartialOrd, Ord)]
enum ImportGroup {
    // the order here defines the order of new group inserts
    Std,
    ExternCrate,
    ThisCrate,
    ThisModule,
    SuperModule,
}

impl ImportGroup {
    fn new(path: &ast::Path) -> ImportGroup {
        let default = ImportGroup::ExternCrate;

        let first_segment = match path.first_segment() {
            Some(it) => it,
            None => return default,
        };

        let kind = first_segment.kind().unwrap_or(PathSegmentKind::SelfKw);
        match kind {
            PathSegmentKind::SelfKw => ImportGroup::ThisModule,
            PathSegmentKind::SuperKw => ImportGroup::SuperModule,
            PathSegmentKind::CrateKw => ImportGroup::ThisCrate,
            PathSegmentKind::Name(name) => match name.text().as_str() {
                "std" => ImportGroup::Std,
                "core" => ImportGroup::Std,
                _ => ImportGroup::ExternCrate,
            },
            PathSegmentKind::Type { .. } => unreachable!(),
        }
    }
}

fn insert_use_(
    scope: &ImportScope,
    insert_path: ast::Path,
    group_imports: bool,
    use_item: ast::Use,
) {
    let scope_syntax = scope.as_syntax_node();
    let group = ImportGroup::new(&insert_path);
    let path_node_iter = scope_syntax
        .children()
        .filter_map(|node| ast::Use::cast(node.clone()).zip(Some(node)))
        .flat_map(|(use_, node)| {
            let tree = use_.use_tree()?;
            let path = tree.path()?;
            let has_tl = tree.use_tree_list().is_some();
            Some((path, has_tl, node))
        });

    if !group_imports {
        if let Some((_, _, node)) = path_node_iter.last() {
            cov_mark::hit!(insert_no_grouping_last);
            ted::insert(ted::Position::after(node), use_item.syntax());
        } else {
            cov_mark::hit!(insert_no_grouping_last2);
            ted::insert(ted::Position::first_child_of(scope_syntax), make::tokens::blank_line());
            ted::insert(ted::Position::first_child_of(scope_syntax), use_item.syntax());
        }
        return;
    }

    // Iterator that discards anything thats not in the required grouping
    // This implementation allows the user to rearrange their import groups as this only takes the first group that fits
    let group_iter = path_node_iter
        .clone()
        .skip_while(|(path, ..)| ImportGroup::new(path) != group)
        .take_while(|(path, ..)| ImportGroup::new(path) == group);

    // track the last element we iterated over, if this is still None after the iteration then that means we never iterated in the first place
    let mut last = None;
    // find the element that would come directly after our new import
    let post_insert: Option<(_, _, SyntaxNode)> = group_iter
        .inspect(|(.., node)| last = Some(node.clone()))
        .find(|&(ref path, has_tl, _)| {
            use_tree_path_cmp(&insert_path, false, path, has_tl) != Ordering::Greater
        });

    if let Some((.., node)) = post_insert {
        cov_mark::hit!(insert_group);
        // insert our import before that element
        return ted::insert(ted::Position::before(node), use_item.syntax());
    }
    if let Some(node) = last {
        cov_mark::hit!(insert_group_last);
        // there is no element after our new import, so append it to the end of the group
        return ted::insert(ted::Position::after(node), use_item.syntax());
    }

    // the group we were looking for actually doesn't exist, so insert

    let mut last = None;
    // find the group that comes after where we want to insert
    let post_group = path_node_iter
        .inspect(|(.., node)| last = Some(node.clone()))
        .find(|(p, ..)| ImportGroup::new(p) > group);
    if let Some((.., node)) = post_group {
        cov_mark::hit!(insert_group_new_group);
        ted::insert(ted::Position::before(&node), use_item.syntax());
        if let Some(node) = algo::non_trivia_sibling(node.into(), Direction::Prev) {
            ted::insert(ted::Position::after(node), make::tokens::single_newline());
        }
        return;
    }
    // there is no such group, so append after the last one
    if let Some(node) = last {
        cov_mark::hit!(insert_group_no_group);
        ted::insert(ted::Position::after(&node), use_item.syntax());
        ted::insert(ted::Position::after(node), make::tokens::single_newline());
        return;
    }
    // there are no imports in this file at all
    if let Some(last_inner_element) = scope_syntax
        .children_with_tokens()
        .filter(|child| match child {
            NodeOrToken::Node(node) => is_inner_attribute(node.clone()),
            NodeOrToken::Token(token) => is_inner_comment(token.clone()),
        })
        .last()
    {
        cov_mark::hit!(insert_group_empty_inner_attr);
        ted::insert(ted::Position::after(&last_inner_element), use_item.syntax());
        ted::insert(ted::Position::after(last_inner_element), make::tokens::single_newline());
        return;
    }
    match scope {
        ImportScope::File(_) => {
            cov_mark::hit!(insert_group_empty_file);
            ted::insert(ted::Position::first_child_of(scope_syntax), make::tokens::blank_line());
            ted::insert(ted::Position::first_child_of(scope_syntax), use_item.syntax())
        }
        // don't insert the imports before the item list's opening curly brace
        ImportScope::Module(item_list) => match item_list.l_curly_token() {
            Some(b) => {
                cov_mark::hit!(insert_group_empty_module);
                ted::insert(ted::Position::after(&b), make::tokens::single_newline());
                ted::insert(ted::Position::after(&b), use_item.syntax());
            }
            None => {
                // This should never happens, broken module syntax node
                ted::insert(
                    ted::Position::first_child_of(scope_syntax),
                    make::tokens::blank_line(),
                );
                ted::insert(ted::Position::first_child_of(scope_syntax), use_item.syntax());
            }
        },
    }
}

fn is_inner_attribute(node: SyntaxNode) -> bool {
    ast::Attr::cast(node).map(|attr| attr.kind()) == Some(ast::AttrKind::Inner)
}

fn is_inner_comment(token: SyntaxToken) -> bool {
    ast::Comment::cast(token).and_then(|comment| comment.kind().doc)
        == Some(ast::CommentPlacement::Inner)
}
#[cfg(test)]
mod tests;
