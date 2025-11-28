//! Task graph construction and dependency resolution
//!
//! Uses petgraph to build a DAG of tasks and perform topological sorting
//! to determine execution order.

use petgraph::algo::{is_cyclic_directed, toposort};
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;

use crate::config::{Config, TaskConfig};
use crate::error::{Result, YatrError};

/// A node in the task graph
#[derive(Debug, Clone)]
pub struct TaskNode {
    pub name: String,
    pub config: TaskConfig,
}

/// The task dependency graph
#[derive(Debug)]
pub struct TaskGraph {
    graph: DiGraph<TaskNode, ()>,
    name_to_index: HashMap<String, NodeIndex>,
}

impl TaskGraph {
    /// Build a task graph from configuration
    pub fn from_config(config: &Config) -> Result<Self> {
        let mut graph = DiGraph::new();
        let mut name_to_index = HashMap::new();

        // Add all tasks as nodes
        for (name, task_config) in &config.tasks {
            let node = TaskNode {
                name: name.clone(),
                config: task_config.clone(),
            };
            let idx = graph.add_node(node);
            name_to_index.insert(name.clone(), idx);
        }

        // Add dependency edges
        for (name, task_config) in &config.tasks {
            let task_idx = name_to_index[name];

            for dep in &task_config.depends {
                let dep_idx = name_to_index.get(dep).ok_or_else(|| YatrError::TaskNotFound {
                    name: dep.clone(),
                    available: config.task_names().iter().map(|s| s.to_string()).collect(),
                })?;

                // Edge goes from dependency TO dependent (dep must run first)
                graph.add_edge(*dep_idx, task_idx, ());
            }
        }

        // Check for cycles
        if is_cyclic_directed(&graph) {
            let cycle = Self::find_cycle_description(&graph, &name_to_index);
            return Err(YatrError::CyclicDependency { cycle });
        }

        Ok(Self {
            graph,
            name_to_index,
        })
    }

    /// Get execution order for a specific task (including dependencies)
    pub fn execution_order(&self, task_name: &str) -> Result<Vec<&TaskNode>> {
        let target_idx = self
            .name_to_index
            .get(task_name)
            .ok_or_else(|| YatrError::TaskNotFound {
                name: task_name.to_string(),
                available: self.name_to_index.keys().cloned().collect(),
            })?;

        // Get all ancestors (dependencies) of the target task
        let required_nodes = self.get_ancestors(*target_idx);

        // Topological sort of the subgraph
        let sorted = toposort(&self.graph, None).map_err(|_| YatrError::CyclicDependency {
            cycle: "Unknown cycle detected".to_string(),
        })?;

        // Filter to only include required nodes, maintaining order
        let execution_order: Vec<&TaskNode> = sorted
            .into_iter()
            .filter(|idx| required_nodes.contains(idx))
            .map(|idx| &self.graph[idx])
            .collect();

        Ok(execution_order)
    }

    /// Get all tasks in dependency order
    pub fn all_tasks_ordered(&self) -> Result<Vec<&TaskNode>> {
        let sorted = toposort(&self.graph, None).map_err(|_| YatrError::CyclicDependency {
            cycle: "Unknown cycle detected".to_string(),
        })?;

        Ok(sorted.into_iter().map(|idx| &self.graph[idx]).collect())
    }

    /// Get ancestors (all dependencies, transitive) of a node
    fn get_ancestors(&self, target: NodeIndex) -> Vec<NodeIndex> {
        use petgraph::visit::Bfs;
        use petgraph::Direction;

        let mut ancestors = vec![target];
        let mut visited = std::collections::HashSet::new();
        visited.insert(target);

        // BFS backwards through dependencies
        let reversed = petgraph::visit::Reversed(&self.graph);
        let mut bfs = Bfs::new(&reversed, target);

        while let Some(node) = bfs.next(&reversed) {
            if visited.insert(node) {
                ancestors.push(node);
            }
        }

        ancestors
    }

    /// Find a human-readable description of a cycle
    fn find_cycle_description(
        graph: &DiGraph<TaskNode, ()>,
        name_to_index: &HashMap<String, NodeIndex>,
    ) -> String {
        // Simple cycle detection for error message
        for (name, &idx) in name_to_index {
            let mut visited = std::collections::HashSet::new();
            let mut path = vec![name.clone()];

            if Self::dfs_find_cycle(graph, idx, idx, &mut visited, &mut path) {
                return path.join(" -> ");
            }
        }

        "Unknown cycle".to_string()
    }

    fn dfs_find_cycle(
        graph: &DiGraph<TaskNode, ()>,
        current: NodeIndex,
        target: NodeIndex,
        visited: &mut std::collections::HashSet<NodeIndex>,
        path: &mut Vec<String>,
    ) -> bool {
        for neighbor in graph.neighbors(current) {
            if neighbor == target && path.len() > 1 {
                path.push(graph[target].name.clone());
                return true;
            }

            if visited.insert(neighbor) {
                path.push(graph[neighbor].name.clone());
                if Self::dfs_find_cycle(graph, neighbor, target, visited, path) {
                    return true;
                }
                path.pop();
            }
        }

        false
    }

    /// Check if a task exists
    pub fn has_task(&self, name: &str) -> bool {
        self.name_to_index.contains_key(name)
    }

    /// Get a task by name
    pub fn get_task(&self, name: &str) -> Option<&TaskNode> {
        self.name_to_index
            .get(name)
            .map(|&idx| &self.graph[idx])
    }

    /// Get all task names
    pub fn task_names(&self) -> impl Iterator<Item = &str> {
        self.name_to_index.keys().map(|s| s.as_str())
    }

    /// Get direct dependencies of a task
    pub fn dependencies(&self, name: &str) -> Option<Vec<&str>> {
        self.name_to_index.get(name).map(|&idx| {
            self.graph
                .neighbors_directed(idx, petgraph::Direction::Incoming)
                .map(|dep_idx| self.graph[dep_idx].name.as_str())
                .collect()
        })
    }

    /// Get tasks that depend on the given task
    pub fn dependents(&self, name: &str) -> Option<Vec<&str>> {
        self.name_to_index.get(name).map(|&idx| {
            self.graph
                .neighbors_directed(idx, petgraph::Direction::Outgoing)
                .map(|dep_idx| self.graph[dep_idx].name.as_str())
                .collect()
        })
    }
}

/// Execution plan for a set of tasks
#[derive(Debug)]
pub struct ExecutionPlan<'a> {
    /// Tasks to execute in order
    pub tasks: Vec<&'a TaskNode>,
    /// Groups of tasks that can run in parallel (respecting dependencies)
    pub parallel_groups: Vec<Vec<&'a TaskNode>>,
}

impl<'a> ExecutionPlan<'a> {
    /// Create an execution plan from a list of tasks
    pub fn from_tasks(tasks: Vec<&'a TaskNode>, graph: &'a TaskGraph) -> Self {
        // Group tasks by "depth" in the dependency graph for parallel execution
        let mut parallel_groups: Vec<Vec<&'a TaskNode>> = Vec::new();
        let mut placed = std::collections::HashSet::new();

        for task in &tasks {
            // Find the earliest group this task can be placed in
            // (all dependencies must be in earlier groups)
            let deps: Vec<_> = graph
                .dependencies(&task.name)
                .unwrap_or_default()
                .into_iter()
                .collect();

            let mut target_group = 0;
            for (i, group) in parallel_groups.iter().enumerate() {
                for t in group {
                    if deps.contains(&t.name.as_str()) {
                        target_group = i + 1;
                    }
                }
            }

            // Ensure we have enough groups
            while parallel_groups.len() <= target_group {
                parallel_groups.push(Vec::new());
            }

            parallel_groups[target_group].push(task);
            placed.insert(&task.name);
        }

        Self {
            tasks,
            parallel_groups,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_config() -> Config {
        let toml = r#"
            [tasks.a]
            run = ["echo a"]

            [tasks.b]
            depends = ["a"]
            run = ["echo b"]

            [tasks.c]
            depends = ["a"]
            run = ["echo c"]

            [tasks.d]
            depends = ["b", "c"]
            run = ["echo d"]
        "#;

        toml::from_str(toml).unwrap()
    }

    #[test]
    fn test_execution_order() {
        let config = make_test_config();
        let graph = TaskGraph::from_config(&config).unwrap();

        let order = graph.execution_order("d").unwrap();
        let names: Vec<_> = order.iter().map(|t| t.name.as_str()).collect();

        // 'a' must come before 'b' and 'c', which must come before 'd'
        assert!(names.iter().position(|&n| n == "a").unwrap() < names.iter().position(|&n| n == "b").unwrap());
        assert!(names.iter().position(|&n| n == "a").unwrap() < names.iter().position(|&n| n == "c").unwrap());
        assert!(names.iter().position(|&n| n == "b").unwrap() < names.iter().position(|&n| n == "d").unwrap());
        assert!(names.iter().position(|&n| n == "c").unwrap() < names.iter().position(|&n| n == "d").unwrap());
    }

    #[test]
    fn test_cycle_detection() {
        let toml = r#"
            [tasks.a]
            depends = ["b"]
            run = ["echo a"]

            [tasks.b]
            depends = ["a"]
            run = ["echo b"]
        "#;

        let config: Config = toml::from_str(toml).unwrap();
        let result = TaskGraph::from_config(&config);

        assert!(matches!(result, Err(YatrError::CyclicDependency { .. })));
    }
}
