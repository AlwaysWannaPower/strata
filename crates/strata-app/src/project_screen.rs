//! # Project screen (Dioxus 0.7): create / open a project, view saved schemas
//!
//! A "project" is a folder holding `project.toml` (+ `schemas/`, `logs/`).
//! This screen manages the *global* project choice, which is owned one level
//! up — in `App` — because both this screen and the Schemas screen ("Save
//! schema to project") need it. Passing a `Signal<Option<PathBuf>>` down as a
//! prop is the Dioxus way to share one piece of state across screens without
//! a global store.
//!
//! The screen itself shows the project metadata and the list of schemas that
//! were saved into it, demonstrating that confirmed schemas survive a restart.

use dioxus::prelude::*;
use std::path::PathBuf;
use strata_core::{open_project, schema_names};

/// Props = the one shared signal this screen may *write* (via its local copy).
#[derive(Props, Clone, PartialEq)]
pub struct ProjectScreenProps {
    /// Current project directory, owned by `App`; shared with Schemas screen.
    project: Signal<Option<PathBuf>>,
}

#[component]
pub fn ProjectScreen(props: ProjectScreenProps) -> Element {
    // `Signal` is Copy but `.set` needs a mutable receiver, so take a local
    // mutable copy (same pattern as NavRail in main.rs).
    let mut project = props.project;
    let mut status = use_signal(String::new);
    let mut name_input = use_signal(String::new);
    let mut schemas = use_signal(|| Option::<Vec<String>>::None);

    // Reload the saved-schema listing for the current project.
    let mut refresh_schemas = move || {
        let Some(dir) = project.read().clone() else {
            *schemas.write() = None;
            return;
        };
        match schema_names(&dir) {
            Ok(names) => *schemas.write() = Some(names),
            Err(err) => status.set(format!("cannot list schemas: {err}")),
        }
    };

    // Create a project at the picked (empty) folder.
    let mut create_at = move |dir: PathBuf| {
        let name = name_input.read().trim().to_string();
        let name = if name.is_empty() {
            dir.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Unnamed".to_string())
        } else {
            name
        };
        match strata_core::create_project(&dir, &name) {
            Ok(meta) => {
                project.set(Some(dir.clone()));
                *schemas.write() = None;
                refresh_schemas();
                status.set(format!(
                    "project created: {} ({})",
                    meta.name,
                    dir.display()
                ));
            }
            Err(err) => status.set(format!("create failed: {err}")),
        }
    };

    // Open an existing project folder.
    let mut open_at = move |dir: PathBuf| match open_project(&dir) {
        Ok(Some(meta)) => {
            project.set(Some(dir.clone()));
            *schemas.write() = None;
            refresh_schemas();
            status.set(format!("project opened: {} ({})", meta.name, dir.display()));
        }
        Ok(None) => status.set(format!(
            "{} has no project.toml — create one first",
            dir.display()
        )),
        Err(err) => status.set(format!("open failed: {err}")),
    };

    rsx! {
        div { class: "screen",
            h1 { class: "screen-title", "Project" }
            p { class: "screen-sub",
                "A project folder keeps project.toml, saved schemas and run logs. "
                "Confirmed schemas (Schemas screen → Save schema) survive a restart."
            }

            div { class: "card",
                div { class: "card-body",
                    div { class: "toolbar",
                        input {
                            class: "path-input",
                            placeholder: "Project name (optional — folder name used if empty)…",
                            value: name_input,
                            oninput: move |evt: Event<FormData>| name_input.set(evt.value()),
                        }
                        button {
                            onclick: move |_| {
                                if let Some(dir) = pick_folder() { create_at(dir); }
                            },
                            "Create project…"
                        }
                        button {
                            onclick: move |_| {
                                if let Some(dir) = pick_folder() { open_at(dir); }
                            },
                            "Open project…"
                        }
                    }
                }
            }

            if let Some(dir) = project.read().as_ref() {
                div { class: "card",
                    div { class: "card-head",
                        span { class: "card-title", "Current project" }
                    }
                    div { class: "card-body",
                        p { class: "mono", "Folder: {dir:?}" }
                        div { class: "toolbar",
                            button { onclick: move |_| refresh_schemas(), "Reload schemas" }
                        }
                        if let Some(schemas) = schemas.read().as_ref() {
                            if schemas.is_empty() {
                                p { class: "empty-note", "No saved schemas yet." }
                            } else {
                                ul { class: "schema-list",
                                    for schema in schemas {
                                        li { "{schema}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            div { class: "status", "{status}" }
        }
    }
}

/// Folder picker used for both create and open.
fn pick_folder() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Choose a project folder")
        .pick_folder()
}
