//! # Preview UI components (Dioxus 0.7)
//!
//! Reusable presentational components for showing a previewed table.
//!
//! ## Dioxus crash-course used in this file
//!
//! A **component** in Dioxus is just a Rust function annotated with
//! `#[component]` that returns an `Element` (= a "virtual node", the Dioxus
//! equivalent of a DOM fragment). Dioxus calls the function to (re)render it
//! whenever its state changes, and diffing figures out what to update — we
//! never touch the DOM by hand.
//!
//! Components that need *input* declare a props struct:
//!
//! ```text
//! #[derive(Props, Clone, PartialEq)]
//! struct MyProps { ... }
//!
//! #[component]
//! fn MyComponent(props: MyProps) -> Element { ... }
//! ```
//!
//! - `Props` (the derive) turns the struct into Dioxus props so callers can
//!   write `<MyComponent field={value} />` in `rsx!`.
//! - `Clone + PartialEq` let Dioxus skip re-rendering when props did not change.
//!
//! Two kinds of children appear below:
//! * plain elements: `table { … }`, `div { … }` — these map to real HTML tags;
//! * components with a capital letter: `PreviewTable { … }`.

use dioxus::prelude::*;
use strata_core::Preview;

/// Props of [`PreviewTable`].
///
/// NOTE on props vs signals: this component is *presentational* — it shows
/// whatever table it is given and holds no state of its own. Therefore it
/// receives the data as a plain value in props. State lives one level up, in
/// the screen that owns it (see `sources.rs`).
#[derive(Props, Clone, PartialEq)]
pub struct PreviewTableProps {
    /// The table to render (columns + rows, already truncated by the caller).
    table: Preview,
}

/// Render a preview table: a header row of column names and the data rows.
///
/// Why an HTML `<table>`: Dioxus desktop renders into a WebView, so every
/// HTML element is available. A plain table is fine up to ~hundreds of rows;
/// the million-row grid is a separate problem (virtualization) planned for M4.
#[component]
pub fn PreviewTable(props: PreviewTableProps) -> Element {
    let table = &props.table;
    rsx! {
        div { class: "table-wrap",
            table { class: "grid",
                // <thead> = the header row built from the column infos.
                thead {
                    tr {
                        // `for column in &table.columns` renders one <th> per
                        // column. `key` is optional here; Dioxus wants it for
                        // *reorderable* lists to track items by identity.
                        for column in &table.columns {
                            th { "{column.name}" }
                        }
                    }
                }
                tbody {
                    // Each data row is a Vec<String>; each cell one <td>.
                    // The text `"{cell}"` inside the braces is *dynamic* text:
                    // Dioxus re-renders it when the value changes.
                    for row in &table.rows {
                        tr {
                            for cell in row {
                                td { "{cell}" }
                            }
                        }
                    }
                }
            }
            // If a file had columns but zero rows we still want a hint.
            if table.rows.is_empty() {
                p { class: "empty-note", "No data rows to show." }
            }
        }
    }
}

/// Props of [`PreviewCard`]: a preview plus a headline (usually the source
/// provenance, e.g. `CSV · delimiter ';' · windows-1251`).
#[derive(Props, Clone, PartialEq)]
pub struct PreviewCardProps {
    /// Headline shown above the grid, e.g. the provenance summary.
    title: String,
    /// The table itself.
    table: Preview,
}

/// A "card": titled panel containing a preview table.
///
/// Composes [`PreviewTable`] — the point of splitting components is reuse:
/// single-file screens, folder-dataset screens and (later) quarantine viewers
/// all show a card like this.
#[component]
pub fn PreviewCard(props: PreviewCardProps) -> Element {
    rsx! {
        div { class: "card",
            div { class: "card-head",
                span { class: "provenance", "{props.title}" }
            }
            PreviewTable { table: props.table }
        }
    }
}

/// Props of [`ColumnsTable`]: plain `(name, type)` rows.
#[derive(Props, Clone, PartialEq)]
pub struct ColumnsTableProps {
    rows: Vec<(String, String)>,
}

/// Render `(column, type)` pairs as a small two-column table.
///
/// Both schema *proposals* (`SchemaColumn`) and *saved schemas* (`ColumnDef`)
/// carry the same name+type shape, so callers map them into pairs and reuse
/// this single presentational component.
#[component]
pub fn ColumnsTable(props: ColumnsTableProps) -> Element {
    rsx! {
        div { class: "table-wrap",
            table { class: "grid",
                thead { tr { th { "Column" } th { "Type" } } }
                tbody {
                    for (name, dtype) in &props.rows {
                        tr {
                            td { "{name}" }
                            td { class: "mono", "{dtype}" }
                        }
                    }
                }
            }
        }
    }
}
