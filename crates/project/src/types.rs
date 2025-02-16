use moon_config::ProjectID;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

pub type TouchedFilePaths = HashSet<PathBuf>;

pub type ExpandedFiles = HashSet<PathBuf>;

pub type EnvVars = HashMap<String, String>;

pub type ProjectsSourceMap = HashMap<ProjectID, String>;
