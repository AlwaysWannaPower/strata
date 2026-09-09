//! # App-level API façade ("ports & adapters", M2 scaffold)
//!
//! Goal (user request): the UI should depend on a *stable application API*,
//! not on engine internals — so that Dioxus can be swapped for another
//! frontend later without touching engine call sites everywhere.
//!
//! Architecture today:
//!
//! ```text
//! screens (dioxus)  →  this API layer  →  strata_core (engine)
//! ```
//!
//! This file is the seam. For now it wraps the workspace/pipeline operations
//! the UI already performs, each returning **engine domain types** (they are
//! plain data — no Polars/Dioxus types leak). As screens migrate onto it, the
//! direct `strata_core::…` imports inside screen files disappear.
//!
//! Later this module may grow its own view types (e.g. `EntityCardViewModel`)
//! so even the engine's domain structs stay internal; that happens together
//! with the frontend refactor.

use std::path::PathBuf;

use strata_core::{
    Binding, ColumnDef, FolderReport, ReaderOptions, SchemaFile, SchemaProposal, WorkspaceConfig,
    create_workspace, open_workspace, save_schema, schema_from_folder, schema_names,
    stage_folder_with_schema,
};

/// Result of a confirmed pipeline action, summarised for UI status lines.
#[derive(Debug, Clone)]
pub enum ApiOutcome {
    /// A workspace was created/opened/updated.
    Workspace(WorkspaceConfig),
    /// A schema was saved; carries the file name.
    SchemaSaved { file: String },
    /// A folder was staged into a dataset.
    Staged(FolderReport),
    /// Nothing else happened (informational message).
    Message(String),
}

/// Create a workspace at `dir` (fails if it already exists).
pub fn create_workspace_at(dir: PathBuf, name: &str) -> Result<WorkspaceConfig, String> {
    create_workspace(&dir, name).map_err(|e| e.to_string())
}

/// Open a workspace: `Ok(None)` = not a workspace folder.
pub fn open_workspace_at(dir: PathBuf) -> Result<Option<WorkspaceConfig>, String> {
    open_workspace(&dir).map_err(|e| e.to_string())
}

/// List the schema files saved in a workspace.
pub fn list_saved_schemas(workspace_dir: &PathBuf) -> Result<Vec<String>, String> {
    schema_names(workspace_dir).map_err(|e| e.to_string())
}

/// Propose a schema for a candidate entity folder (never writes anything).
pub fn propose_folder_schema(folder: &PathBuf) -> Result<SchemaProposal, String> {
    let report = schema_from_folder(folder, ReaderOptions::default()).map_err(|e| e.to_string())?;
    Ok(SchemaProposal {
        columns: report.columns,
    })
}

/// Confirmed schema + binding: infer, save the schema file, register binding.
pub fn confirm_entity(
    workspace_dir: &PathBuf,
    entity: &str,
    folder: &PathBuf,
) -> Result<ApiOutcome, String> {
    let report = schema_from_folder(folder, ReaderOptions::default()).map_err(|e| e.to_string())?;
    let columns: Vec<ColumnDef> = report
        .columns
        .into_iter()
        .map(|c| ColumnDef {
            name: c.name,
            dtype: c.dtype,
        })
        .collect();
    let schema = SchemaFile::new(folder.clone(), ReaderOptions::default(), columns);
    let file = save_schema(workspace_dir, &schema).map_err(|e| e.to_string())?;

    // Bind into the workspace config (read-modify-write).
    let mut config = open_workspace_at(workspace_dir.clone())?
        .ok_or_else(|| String::from("workspace.toml missing"))?;
    strata_core::upsert_binding(
        workspace_dir,
        &mut config,
        entity.to_string(),
        folder.clone(),
    )
    .map_err(|e| e.to_string())?;
    Ok(ApiOutcome::SchemaSaved { file })
}

/// Stage a bound entity folder under its confirmed schema.
pub fn stage_entity_typed(
    workspace_dir: &PathBuf,
    binding: &Binding,
    data_dir: PathBuf,
) -> Result<ApiOutcome, String> {
    let config = open_workspace_at(workspace_dir.clone())?
        .ok_or_else(|| String::from("workspace.toml missing"))?;
    let _ = &config;
    // Schema file name == entity name.
    let schema_name = format!("{}.toml", binding.entity);
    let schema =
        strata_core::load_schema(workspace_dir, &schema_name).map_err(|e| e.to_string())?;
    let dest = data_dir.join(&binding.entity);
    let folder = PathBuf::from(&binding.folder);
    let report = stage_folder_with_schema(&folder, &dest, &schema).map_err(|e| e.to_string())?;
    Ok(ApiOutcome::Staged(report))
}
