//! In-memory memory backend with an entity/relationship graph.
//!
//! Same `Vec`-backed document store and word-overlap search as
//! [`super::local_backend::LocalMemoryBackend`], plus a small graph:
//! entities named at write time become nodes, relationship objects become
//! typed edges. This is the storage half of
//! `docs/notes/ideas/graph-memory.md`; the document-plus-graph ranking
//! merge is deliberately not here, so `search_memories` ignores
//! `include_related` exactly like the local backend does.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::backend::{GraphBackend, MemoryBackend, MemorySearchResult};
use super::local_backend::LocalMemoryBackend;
use super::models::Memory;

/// One typed edge between two entity names.
#[derive(Debug, Clone)]
struct GraphEdge {
    source: String,
    target: String,
    relation_type: String,
    weight: f64,
}

/// Entity names seen at write time, each with the ids of the memories that
/// named it, plus the typed edges between them.
///
/// Single-process and unscoped like the document store beside it:
/// `query_subgraph` takes no user, so entities from every user share one
/// graph. Entities and edges outlive any one memory; deletes only drop the
/// id refs. Names match exactly (case-sensitive).
#[derive(Debug, Default)]
struct MemoryGraph {
    entities: HashMap<String, Vec<String>>,
    edges: Vec<GraphEdge>,
}

/// Parse one relationship object. Accepts upstream's tool-call keys
/// (`source`/`target`, `relation` or `relation_type`, optional `weight`);
/// anything else is skipped rather than failing the save.
fn parse_edge(value: &Value) -> Option<GraphEdge> {
    let obj = value.as_object()?;
    let source = obj.get("source")?.as_str()?;
    let target = obj.get("target")?.as_str()?;
    let relation_type = obj
        .get("relation_type")
        .or_else(|| obj.get("relation"))
        .and_then(Value::as_str)
        .unwrap_or("related_to");
    let weight = obj.get("weight").and_then(Value::as_f64).unwrap_or(1.0);
    Some(GraphEdge {
        source: source.to_string(),
        target: target.to_string(),
        relation_type: relation_type.to_string(),
        weight,
    })
}

/// In-memory memory backend with graph support. Same durability caveats as
/// [`LocalMemoryBackend`]: everything is lost on restart.
pub struct GraphMemoryBackend {
    memories: Arc<tokio::sync::RwLock<Vec<Memory>>>,
    graph: Arc<tokio::sync::RwLock<MemoryGraph>>,
}

impl GraphMemoryBackend {
    pub fn new() -> Self {
        Self {
            memories: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            graph: Arc::new(tokio::sync::RwLock::new(MemoryGraph::default())),
        }
    }

    /// Drop deleted memory ids from every entity's posting list.
    async fn scrub_ids(&self, removed: &[String]) {
        if removed.is_empty() {
            return;
        }
        let mut graph = self.graph.write().await;
        for ids in graph.entities.values_mut() {
            ids.retain(|id| !removed.contains(id));
        }
    }
}

impl Default for GraphMemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MemoryBackend for GraphMemoryBackend {
    async fn save_memory(
        &self,
        content: &str,
        user_id: &str,
        importance: f64,
        _facts: Option<&[String]>,
        entities: Option<&[String]>,
        _extracted_entities: Option<&[serde_json::Value]>,
        relationships: Option<&[serde_json::Value]>,
        extracted_relationships: Option<&[serde_json::Value]>,
    ) -> Result<Memory, Box<dyn std::error::Error + Send + Sync>> {
        let mut memory = Memory {
            content: content.to_string(),
            user_id: user_id.to_string(),
            importance,
            ..Memory::default()
        };
        if let Some(ents) = entities {
            memory.entity_refs = ents.to_vec();
        }
        let mut store = self.memories.write().await;
        store.push(memory.clone());
        drop(store);

        let mut graph = self.graph.write().await;
        if let Some(ents) = entities {
            for entity in ents {
                graph
                    .entities
                    .entry(entity.clone())
                    .or_default()
                    .push(memory.id.clone());
            }
        }
        for rels in [relationships, extracted_relationships]
            .into_iter()
            .flatten()
        {
            for rel in rels {
                if let Some(edge) = parse_edge(rel) {
                    graph.entities.entry(edge.source.clone()).or_default();
                    graph.entities.entry(edge.target.clone()).or_default();
                    graph.edges.push(edge);
                }
            }
        }
        Ok(memory)
    }

    async fn search_memories(
        &self,
        query: &str,
        user_id: &str,
        top_k: usize,
        _include_related: bool,
    ) -> Result<Vec<MemorySearchResult>, Box<dyn std::error::Error + Send + Sync>> {
        let store = self.memories.read().await;
        let mut scored: Vec<MemorySearchResult> = store
            .iter()
            .filter(|m| m.user_id == user_id && m.is_current())
            .map(|m| {
                let score = LocalMemoryBackend::text_relevance(query, &m.content);
                MemorySearchResult {
                    memory: m.clone(),
                    score,
                    related_entities: Vec::new(),
                }
            })
            .filter(|r| r.score > 0.0)
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(top_k);
        Ok(scored)
    }

    async fn update_memory(
        &self,
        memory_id: &str,
        new_content: &str,
        _user_id: &str,
        _reason: Option<&str>,
    ) -> Result<Memory, Box<dyn std::error::Error + Send + Sync>> {
        let mut store = self.memories.write().await;
        if let Some(m) = store.iter_mut().find(|m| m.id == memory_id) {
            m.content = new_content.to_string();
            Ok(m.clone())
        } else {
            Err(format!("Memory {memory_id} not found").into())
        }
    }

    async fn delete_memory(
        &self,
        memory_id: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let mut store = self.memories.write().await;
        let before = store.len();
        store.retain(|m| m.id != memory_id);
        let removed = store.len() < before;
        drop(store);
        if removed {
            self.scrub_ids(&[memory_id.to_string()]).await;
        }
        Ok(removed)
    }

    async fn get_memory(
        &self,
        memory_id: &str,
    ) -> Result<Option<Memory>, Box<dyn std::error::Error + Send + Sync>> {
        let store = self.memories.read().await;
        Ok(store.iter().find(|m| m.id == memory_id).cloned())
    }

    async fn clear_user(
        &self,
        user_id: &str,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let mut store = self.memories.write().await;
        let before = store.len();
        let removed: Vec<String> = store
            .iter()
            .filter(|m| m.user_id == user_id)
            .map(|m| m.id.clone())
            .collect();
        store.retain(|m| m.user_id != user_id);
        let cleared = before - store.len();
        drop(store);
        self.scrub_ids(&removed).await;
        Ok(cleared)
    }

    async fn close(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
}

#[async_trait]
impl GraphBackend for GraphMemoryBackend {
    async fn query_subgraph(
        &self,
        entities: &[String],
        depth: usize,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let graph = self.graph.read().await;
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        for seed in entities {
            if visited.insert(seed.clone()) {
                queue.push_back((seed.clone(), 0));
            }
        }
        let mut edge_idx: HashSet<usize> = HashSet::new();
        while let Some((name, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            for (i, edge) in graph.edges.iter().enumerate() {
                // Undirected on purpose: a seed matching either endpoint
                // covers both directions in one comparison, with no OR to
                // misplace (upstream #3236 was exactly that misplaced OR).
                let next = if edge.source == name {
                    &edge.target
                } else if edge.target == name {
                    &edge.source
                } else {
                    continue;
                };
                edge_idx.insert(i);
                if visited.insert(next.clone()) {
                    queue.push_back((next.clone(), d + 1));
                }
            }
        }

        let mut names: Vec<&String> = visited.iter().collect();
        names.sort();
        let nodes: Vec<Value> = names
            .iter()
            .map(|name| {
                let ids = graph.entities.get(*name).cloned().unwrap_or_default();
                serde_json::json!({"entity": name, "memory_ids": ids})
            })
            .collect();
        let mut idx: Vec<usize> = edge_idx.into_iter().collect();
        idx.sort_unstable();
        let edges: Vec<Value> = idx
            .iter()
            .map(|&i| {
                let edge = &graph.edges[i];
                serde_json::json!({
                    "source": edge.source,
                    "target": edge.target,
                    "relation_type": edge.relation_type,
                    "weight": edge.weight,
                })
            })
            .collect();
        Ok(serde_json::json!({"nodes": nodes, "edges": edges}))
    }

    fn supports_graph(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(source: &str, target: &str, relation: &str) -> Value {
        serde_json::json!({"source": source, "target": target, "relation": relation})
    }

    #[tokio::test]
    async fn supports_graph_is_true() {
        assert!(GraphMemoryBackend::new().supports_graph());
    }

    #[tokio::test]
    async fn save_records_entities_and_edges() {
        let backend = GraphMemoryBackend::new();
        let entities = vec!["Alice".to_string(), "Acme".to_string()];
        let rels = [edge("Alice", "Acme", "works_at")];
        let m = backend
            .save_memory(
                "Alice works at Acme",
                "alice",
                0.8,
                None,
                Some(&entities),
                None,
                Some(&rels),
                None,
            )
            .await
            .unwrap();

        let seeds = vec!["Alice".to_string()];
        let sub = backend.query_subgraph(&seeds, 1).await.unwrap();
        let nodes = sub["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 2);
        let alice = nodes.iter().find(|n| n["entity"] == "Alice").unwrap();
        assert_eq!(alice["memory_ids"][0], m.id);
        let edges = sub["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["relation_type"], "works_at");

        // Document search still works beside the graph.
        let results = backend
            .search_memories("works at", "alice", 10, false)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn subgraph_respects_depth() {
        let backend = GraphMemoryBackend::new();
        let rels = [
            edge("Alice", "Bob", "knows"),
            edge("Bob", "Acme", "works_at"),
        ];
        backend
            .save_memory("chain", "alice", 0.5, None, None, None, Some(&rels), None)
            .await
            .unwrap();

        let seeds = vec!["Alice".to_string()];
        let one = backend.query_subgraph(&seeds, 1).await.unwrap();
        assert_eq!(one["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(one["edges"].as_array().unwrap().len(), 1);

        let two = backend.query_subgraph(&seeds, 2).await.unwrap();
        assert_eq!(two["nodes"].as_array().unwrap().len(), 3);
        assert_eq!(two["edges"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn unparseable_relationships_are_skipped() {
        let backend = GraphMemoryBackend::new();
        let rels = [
            serde_json::json!({"source": "Alice"}),
            serde_json::json!("not-an-object"),
        ];
        backend
            .save_memory("note", "alice", 0.5, None, None, None, Some(&rels), None)
            .await
            .unwrap();

        let seeds = vec!["Alice".to_string()];
        let sub = backend.query_subgraph(&seeds, 1).await.unwrap();
        assert_eq!(sub["nodes"].as_array().unwrap().len(), 1);
        assert!(sub["edges"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_scrubs_memory_refs() {
        let backend = GraphMemoryBackend::new();
        let entities = vec!["Alice".to_string()];
        let m = backend
            .save_memory(
                "note",
                "alice",
                0.5,
                None,
                Some(&entities),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(backend.delete_memory(&m.id).await.unwrap());

        let seeds = vec!["Alice".to_string()];
        let sub = backend.query_subgraph(&seeds, 0).await.unwrap();
        let nodes = sub["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0]["memory_ids"].as_array().unwrap().is_empty());
    }
}
