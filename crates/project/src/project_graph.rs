use crate::constants::ROOT_NODE_ID;
use crate::errors::ProjectError;
use crate::helpers::detect_projects_with_globs;
use crate::project::Project;
use crate::types::ProjectsSourceMap;
use moon_cache::CacheEngine;
use moon_config::constants::FLAG_PROJECTS_USING_GLOB;
use moon_config::{GlobalProjectConfig, ProjectID, WorkspaceConfig};
use moon_logger::{color, debug, map_list, trace};
use petgraph::dot::{Config, Dot};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockWriteGuard};

type GraphType = DiGraph<Project, ()>;
type IndicesType = HashMap<ProjectID, NodeIndex>;

const LOG_TARGET: &str = "moon:project-graph";
const READ_ERROR: &str = "Failed to acquire a read lock";
const WRITE_ERROR: &str = "Failed to acquire a write lock";

async fn load_projects_from_cache(
    workspace_root: &Path,
    projects: &ProjectsSourceMap,
    engine: &CacheEngine,
) -> Result<ProjectsSourceMap, ProjectError> {
    // Projects were mapped manually and are not using globs
    if !projects.contains_key(FLAG_PROJECTS_USING_GLOB) {
        return Ok(projects.clone());
    }

    let mut cache = engine.cache_projects_state().await?;

    // Return the values from the cache
    if !cache.item.projects.is_empty() {
        debug!(target: LOG_TARGET, "Loading projects from cache");

        return Ok(cache.item.projects);
    }

    // Extract globs from our fake projects map
    let globs = projects
        .iter()
        .filter_map(|(key, value)| {
            if key == FLAG_PROJECTS_USING_GLOB {
                None
            } else {
                Some(value.clone())
            }
        })
        .collect::<Vec<String>>();

    debug!(
        target: LOG_TARGET,
        "Finding projects with globs: {}",
        map_list(&globs, |g| color::file(g))
    );

    // Generate a new projects map by globbing the filesystem
    let mut map = HashMap::new();

    detect_projects_with_globs(workspace_root, &globs, &mut map)?;

    // Update the cache
    cache.item.globs = globs;
    cache.item.projects = map.clone();
    cache.save().await?;

    Ok(map)
}

pub struct ProjectGraph {
    /// The global project configuration that all projects inherit from.
    /// Is loaded from `.moon/project.yml`.
    global_config: GlobalProjectConfig,

    /// Projects that have been loaded into scope represented as a DAG.
    graph: Arc<RwLock<GraphType>>,

    /// Inputs to be inherited by all tasks.
    implicit_inputs: Vec<String>,

    /// Mapping of project IDs to node indices, as we need a way
    /// to query the graph by ID as it only supports it by index.
    indices: Arc<RwLock<IndicesType>>,

    /// The mapping of projects by ID to a relative file system location.
    /// Is the `projects` setting in `.moon/workspace.yml`.
    projects_map: HashMap<ProjectID, String>,

    /// The workspace root, in which projects are relatively loaded from.
    workspace_root: PathBuf,
}

impl ProjectGraph {
    pub async fn create(
        workspace_root: &Path,
        workspace_config: &WorkspaceConfig,
        global_config: GlobalProjectConfig,
        cache: &CacheEngine,
    ) -> Result<ProjectGraph, ProjectError> {
        debug!(
            target: LOG_TARGET,
            "Creating project graph with {} projects",
            workspace_config.projects.len(),
        );

        let mut graph = DiGraph::new();

        // Add a virtual root node
        graph.add_node(Project {
            id: ROOT_NODE_ID.to_owned(),
            root: workspace_root.to_path_buf(),
            source: String::from("."),
            ..Project::default()
        });

        Ok(ProjectGraph {
            global_config,
            graph: Arc::new(RwLock::new(graph)),
            implicit_inputs: workspace_config.action_runner.implicit_inputs.clone(),
            indices: Arc::new(RwLock::new(HashMap::new())),
            projects_map: load_projects_from_cache(
                workspace_root,
                &workspace_config.projects,
                cache,
            )
            .await?,
            workspace_root: workspace_root.to_path_buf(),
        })
    }

    /// Return a list of all configured project IDs in ascending order.
    pub fn ids(&self) -> Vec<ProjectID> {
        let mut nodes: Vec<ProjectID> = self.projects_map.keys().cloned().collect();
        nodes.sort();
        nodes
    }

    /// Return a project with the associated ID. If the project
    /// has not been loaded, it will be loaded and inserted into the
    /// project graph. If the project does not exist or has been
    /// misconfigured, an error will be returned.
    #[track_caller]
    pub fn load(&self, id: &str) -> Result<Project, ProjectError> {
        // Check if the project already exists in read-only mode,
        // so that it may be dropped immediately after!
        {
            let indices = self.indices.read().expect(READ_ERROR);

            if let Some(index) = indices.get(id) {
                let graph = self.graph.read().expect(READ_ERROR);

                return Ok(graph.node_weight(*index).unwrap().clone());
            }
        }

        // Otherwise we need to load the project in write mode
        let mut indices = self.indices.write().expect(WRITE_ERROR);
        let mut graph = self.graph.write().expect(WRITE_ERROR);
        let index = self.internal_load(id, &mut indices, &mut graph)?;

        Ok(graph.node_weight(index).unwrap().clone())
    }

    /// Force load all projects into the graph. This is necessary
    /// when needing to access project *dependents*, and may also
    /// be a costly operation!
    #[track_caller]
    pub fn load_all(&self) -> Result<(), ProjectError> {
        let mut indices = self.indices.write().expect(WRITE_ERROR);
        let mut graph = self.graph.write().expect(WRITE_ERROR);

        for id in self.ids() {
            self.internal_load(&id, &mut indices, &mut graph)?;
        }

        Ok(())
    }

    /// Return a list of direct project IDs that the defined project depends on.
    #[track_caller]
    pub fn get_dependencies_of(&self, project: &Project) -> Result<Vec<ProjectID>, ProjectError> {
        let indices = self.indices.read().expect(READ_ERROR);
        let graph = self.graph.read().expect(READ_ERROR);

        let deps = graph
            .neighbors_directed(*indices.get(&project.id).unwrap(), Direction::Outgoing)
            .map(|idx| graph.node_weight(idx).unwrap().id.clone())
            .collect();

        Ok(deps)
    }

    /// Return a list of project IDs that require the defined project.
    #[track_caller]
    pub fn get_dependents_of(&self, project: &Project) -> Result<Vec<ProjectID>, ProjectError> {
        let indices = self.indices.read().expect(READ_ERROR);
        let graph = self.graph.read().expect(READ_ERROR);

        let deps = graph
            .neighbors_directed(*indices.get(&project.id).unwrap(), Direction::Incoming)
            .map(|idx| graph.node_weight(idx).unwrap().id.clone())
            .filter(|id| id != ROOT_NODE_ID)
            .collect();

        Ok(deps)
    }

    /// Format as a DOT string.
    pub fn to_dot(&self) -> String {
        let graph = self.graph.read().expect(READ_ERROR);
        let labeled_graph = graph.map(|_, n| n.id.clone(), |_, e| e);
        // let highlight_id = highlight_id.clone().unwrap_or_default();

        let dot = Dot::with_attr_getters(
            &labeled_graph,
            &[Config::EdgeNoLabel, Config::NodeNoLabel],
            &|_, e| {
                if e.source().index() == 0 {
                    String::from("arrowhead=none")
                } else {
                    String::from("arrowhead=box, arrowtail=box")
                }
            },
            &|_, n| {
                let id = n.1;

                if id == ROOT_NODE_ID {
                    format!(
                        "label=\"{}\" style=filled, shape=oval, fillcolor=black, fontcolor=white",
                        id
                    )
                // } else if id == &highlight_id {
                //     String::from("style=filled, shape=circle, fillcolor=palegreen, fontcolor=black")
                } else {
                    format!(
                        "label=\"{}\" style=filled, shape=oval, fillcolor=gray, fontcolor=black",
                        id
                    )
                }
            },
        );

        format!("{:?}", dot)
    }

    /// Internal method for lazily loading a project and its
    /// dependencies into the graph.
    fn internal_load(
        &self,
        id: &str,
        indices: &mut RwLockWriteGuard<IndicesType>,
        graph: &mut RwLockWriteGuard<GraphType>,
    ) -> Result<NodeIndex, ProjectError> {
        // Already loaded, abort early
        if indices.contains_key(id) || id == ROOT_NODE_ID {
            trace!(
                target: LOG_TARGET,
                "Project {} already exists in the project graph",
                color::id(id),
            );

            return Ok(*indices.get(id).unwrap());
        }

        trace!(
            target: LOG_TARGET,
            "Project {} does not exist in the project graph, attempting to load",
            color::id(id),
        );

        // Create project based on ID and source
        let source = match self.projects_map.get(id) {
            Some(path) => path,
            None => return Err(ProjectError::UnconfiguredID(String::from(id))),
        };

        let project = Project::new(
            id,
            source,
            &self.workspace_root,
            &self.global_config,
            &self.implicit_inputs,
        )?;
        let depends_on = project.get_dependencies();

        // Insert the project into the graph
        let node_index = graph.add_node(project);
        graph.add_edge(NodeIndex::new(0), node_index, ());
        indices.insert(id.to_owned(), node_index);

        if !depends_on.is_empty() {
            trace!(
                target: LOG_TARGET,
                "Adding dependencies {} to project {}",
                map_list(&depends_on, |d| color::symbol(d)),
                color::id(id),
            );

            for dep_id in depends_on {
                let dep_index = self.internal_load(dep_id.as_str(), indices, graph)?;
                graph.add_edge(node_index, dep_index, ());
            }
        }

        Ok(node_index)
    }
}
