//! Active tree-path resolution for append-only session trees.
//!
//! A session file is an append-only log of entries. Every non-header entry
//! carries an `id` and an optional `parentId` naming its predecessor, so the
//! file holds the full tree of every branch ever appended. Branches are
//! siblings sharing a parent. There is no persisted leaf pointer: the active
//! branch is reconstructed by walking the `parentId` chain backward from the
//! last-appended entry to the root, then reversing it to chronological order.
//!
//! Resolution is defensive: a cycle in the chain is guarded so the walk
//! terminates, and a parent id that is absent from the entry set truncates the
//! path to the reachable suffix. Projection is lossy — only recognized
//! `user`/`assistant` first text survives, via the shared `parsed_message`
//! helper, so non-message entries (model/thinking changes, compactions, …)
//! shape the tree without emitting a message.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::domain::Message;
use crate::formats::parsed_message;

/// One non-header entry projected for tree resolution and message projection.
///
/// `role`/`content`/`timestamp` are extracted up front by the adapter. Only
/// entries yielding a recognized `Role` survive projection, so non-message
/// entries simply contribute to the tree shape without emitting a message.
#[derive(Debug, Clone, Copy)]
pub struct TreeNode<'a> {
    pub id: &'a str,
    pub parent_id: Option<&'a str>,
    pub role: Option<&'a str>,
    pub content: Option<&'a Value>,
    pub timestamp: Option<&'a str>,
}

/// Resolve the active branch as an ordered slice of nodes, root → leaf.
///
/// The active leaf is the last node (in file/append order) carrying a non-empty
/// `id`. The path is rebuilt by following `parentId` from the leaf toward the
/// root, with a cycle guard, and stops at a missing parent — the reachable
/// suffix up to (and including) that point is returned. An empty input, or one
/// with no id-bearing entry, yields an empty path.
pub fn active_path<'a>(nodes: &'a [TreeNode<'a>]) -> Vec<&'a TreeNode<'a>> {
    let by_id: HashMap<&str, &TreeNode<'a>> = nodes
        .iter()
        .filter(|node| !node.id.is_empty())
        .map(|node| (node.id, node))
        .collect();

    let Some(leaf) = nodes.iter().rev().find(|node| !node.id.is_empty()) else {
        return Vec::new();
    };

    let mut path: Vec<&TreeNode<'a>> = Vec::new();
    let mut visited: HashSet<&str> = HashSet::new();
    let mut cursor: Option<&TreeNode<'a>> = Some(leaf);
    while let Some(node) = cursor {
        // Cycle guard: once a node id reappears the chain is corrupt; stop
        // without re-emitting it, keeping the reachable suffix.
        if !visited.insert(node.id) {
            break;
        }
        path.push(node);
        cursor = match node.parent_id {
            None => None,
            Some(parent_id) => by_id.get(parent_id).copied(),
        };
    }

    path.reverse();
    path
}

/// Project an ordered (root → leaf) path of nodes into first-text `Message`s,
/// keeping only recognized `user`/`assistant` roles.
///
/// Projection is lossy: unrecognized roles, empty content, and non-message
/// entries are dropped. The shared `parsed_message` helper performs the role
/// parse and first-text extraction, so this stays a thin, ordered filter.
pub fn project_messages<'a>(path: &[&'a TreeNode<'a>]) -> Vec<Message> {
    path.iter()
        .filter_map(|node| parsed_message(node.role, node.content, node.timestamp))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn node<'a>(
        id: &'a str,
        parent_id: Option<&'a str>,
        role: Option<&'a str>,
        content: Option<&'a Value>,
        timestamp: Option<&'a str>,
    ) -> TreeNode<'a> {
        TreeNode {
            id,
            parent_id,
            role,
            content,
            timestamp,
        }
    }

    fn ids<'a>(path: &[&'a TreeNode<'a>]) -> Vec<&'a str> {
        path.iter().map(|node| node.id).collect()
    }

    fn roles<'a>(path: &[&'a TreeNode<'a>]) -> Vec<&'a str> {
        path.iter().filter_map(|node| node.role).collect()
    }

    #[test]
    fn active_path_empty_when_no_id_bearing_nodes() {
        assert!(active_path(&[]).is_empty());

        let nodes = [node("", None, None, None, None)];
        assert!(active_path(&nodes).is_empty());
    }

    #[test]
    fn active_path_linear_chain_root_to_leaf() {
        let c1 = json!([{"type": "text", "text": "one"}]);
        let c2 = json!([{"type": "text", "text": "two"}]);
        let nodes = [
            node("a", None, Some("user"), Some(&c1), Some("t1")),
            node("b", Some("a"), Some("assistant"), Some(&c2), Some("t2")),
        ];
        let path = active_path(&nodes);
        assert_eq!(ids(&path), ["a", "b"]);
    }

    #[test]
    fn active_branch_picks_last_appended_leaf_path() {
        // Two assistant siblings branch off root `r`: `a` then `b`. The last
        // appended leaf `c` descends from `b`, so the active path is r→b→c and
        // the dead `a` branch is excluded.
        let rc = json!([{"type": "text", "text": "root"}]);
        let ac = json!([{"type": "text", "text": "a-msg"}]);
        let bc = json!([{"type": "text", "text": "b-msg"}]);
        let cc = json!([{"type": "text", "text": "c-msg"}]);
        let nodes = [
            node("r", None, Some("user"), Some(&rc), Some("t0")),
            node("a", Some("r"), Some("assistant"), Some(&ac), Some("t1")),
            node("b", Some("r"), Some("assistant"), Some(&bc), Some("t2")),
            node("c", Some("b"), Some("user"), Some(&cc), Some("t3")),
        ];
        let path = active_path(&nodes);
        assert_eq!(ids(&path), ["r", "b", "c"]);
        assert_eq!(roles(&path), ["user", "assistant", "user"]);
    }

    #[test]
    fn active_path_ignores_dead_branch_appended_earlier() {
        // Leaf `a` was appended, then a sibling branch `r→b→c` was appended
        // later; the later leaf wins and the earlier `a` branch is excluded.
        let ac = json!([{"type": "text", "text": "a"}]);
        let bc = json!([{"type": "text", "text": "b"}]);
        let cc = json!([{"type": "text", "text": "c"}]);
        let nodes = [
            node("a", None, Some("user"), Some(&ac), Some("t1")),
            node("b", None, Some("user"), Some(&bc), Some("t2")),
            node("c", Some("b"), Some("assistant"), Some(&cc), Some("t3")),
        ];
        let path = active_path(&nodes);
        assert_eq!(ids(&path), ["b", "c"]);
    }

    #[test]
    fn cycle_guard_terminates_and_returns_reachable_suffix() {
        // a → b → a (a.parentId = b, b.parentId = a). Walking from the last
        // leaf `b` must terminate and yield the non-repeating suffix.
        let nodes = [
            node("a", Some("b"), Some("user"), None, None),
            node("b", Some("a"), Some("assistant"), None, None),
        ];
        let path = active_path(&nodes);
        assert_eq!(ids(&path), ["a", "b"]);
    }

    #[test]
    fn self_cycle_yields_single_node_suffix() {
        let nodes = [node("a", Some("a"), Some("user"), None, None)];
        let path = active_path(&nodes);
        assert_eq!(ids(&path), ["a"]);
    }

    #[test]
    fn missing_parent_truncates_to_reachable_suffix() {
        // Leaf `c` points at a parent `ghost` absent from the entry set; the
        // path is the single reachable node `c`.
        let cc = json!([{"type": "text", "text": "c"}]);
        let nodes = [node("c", Some("ghost"), Some("user"), Some(&cc), Some("t1"))];
        let path = active_path(&nodes);
        assert_eq!(ids(&path), ["c"]);
    }

    #[test]
    fn missing_parent_mid_chain_keeps_suffix_to_orphan() {
        // r and c exist, but c.parentId `p` is absent; walking from leaf c
        // stops at the orphan, so the reachable suffix is just c (r is never
        // linked into the leaf chain).
        let cc = json!([{"type": "text", "text": "c"}]);
        let rc = json!([{"type": "text", "text": "r"}]);
        let nodes = [
            node("r", None, Some("user"), Some(&rc), Some("t0")),
            node("c", Some("p"), Some("assistant"), Some(&cc), Some("t2")),
        ];
        let path = active_path(&nodes);
        assert_eq!(ids(&path), ["c"]);
    }

    #[test]
    fn null_parent_marks_root() {
        let c1 = json!([{"type": "text", "text": "root"}]);
        let nodes = [node("root", None, Some("user"), Some(&c1), Some("t0"))];
        let path = active_path(&nodes);
        assert_eq!(ids(&path), ["root"]);
    }

    #[test]
    fn project_messages_keeps_user_assistant_first_text_in_order() {
        let u = json!([{"type": "text", "text": "hello"}]);
        let a = json!([{"type": "text", "text": "hi there"}]);
        let nodes = [
            node("m1", None, Some("user"), Some(&u), Some("t1")),
            node("m2", Some("m1"), Some("assistant"), Some(&a), Some("t2")),
        ];
        let path = active_path(&nodes);
        let messages = project_messages(&path);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "hello");
        assert_eq!(messages[0].timestamp.as_deref(), Some("t1"));
        assert_eq!(messages[1].text, "hi there");
    }

    #[test]
    fn project_messages_drops_unrecognized_roles_and_non_message_entries() {
        // toolResult / developer roles and a non-message entry (no role) sit
        // on the path but must not survive projection.
        let u = json!([{"type": "text", "text": "q"}]);
        let a = json!([{"type": "text", "text": "ans"}]);
        let tr = json!([{"type": "text", "text": "tool out"}]);
        let dev = json!("dev");
        let nodes = [
            node("m1", None, Some("user"), Some(&u), Some("t1")),
            node("m2", Some("m1"), Some("assistant"), Some(&a), Some("t2")),
            node("m3", Some("m2"), Some("toolResult"), Some(&tr), Some("t3")),
            node("m4", Some("m3"), None, None, Some("t4")),
            node("m5", Some("m4"), Some("developer"), Some(&dev), Some("t5")),
        ];
        let path = active_path(&nodes);
        let messages = project_messages(&path);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "q");
        assert_eq!(messages[1].text, "ans");
    }

    #[test]
    fn project_string_content_takes_first_text() {
        let u = json!("plain string content");
        let nodes = [node("m1", None, Some("user"), Some(&u), Some("t1"))];
        let path = active_path(&nodes);
        let messages = project_messages(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "plain string content");
    }

    #[test]
    fn project_skips_empty_text() {
        let u = json!([{"type": "text", "text": ""}]);
        let nodes = [node("m1", None, Some("user"), Some(&u), Some("t1"))];
        let path = active_path(&nodes);
        let messages = project_messages(&path);
        assert!(messages.is_empty());
    }
}