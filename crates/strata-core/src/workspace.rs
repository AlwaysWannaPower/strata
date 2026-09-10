//! # Модель воркспейса (M1c): папка настроек в духе IDE + вывод в `data/`
//!
//! Раскладка (см. `docs/design-workspace-pipeline.md`):
//!
//! ```text
//! <workspace>/
//! ├── workspace.toml     # имя, data_dir, привязки (папка→сущность), scan roots
//! ├── schemas/           # *.schema.toml — подтверждённые схемы (хелперы project.rs)
//! ├── data/<entity>/     # сюда пишутся части Parquet (data_dir по умолчанию = "data")
//! ├── plugins/           # зарезервировано
//! └── logs/              # зарезервировано
//! ```
//!
//! Главное правило: **одна папка = одна схема**. Два режима привязки:
//!
//! * явный [`Binding`] — пользователь сказал «папка X это сущность `sales`»;
//! * scan roots — пользователь дал корень; его **прямые подпапки** — кандидаты
//!   в сущности (режим B). Корни запоминаются здесь; кандидаты находятся во
//!   время скана и становятся явными привязками только после подтверждения
//!   пользователем.
//!
//! Сами схемы хранит и загружает существующий набор хелперов `project.rs`
//! (формат файла схемы общий); этот модуль владеет только *конфигурацией
//! воркспейса*.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::StrataError;

/// Версия формата файла конфигурации.
const CONFIG_FORMAT: u32 = 1;

/// Одна подтверждённая привязка: папка, файлы которой принадлежат одной сущности/схеме.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    /// Имя логической таблицы (оно же имя файла схемы и директория `data/<entity>/`).
    pub entity: String,
    /// Абсолютный путь папки с файлами сущности.
    pub folder: String,
    /// Когда привязка была добавлена (UTC, для справки).
    pub added_utc: String,
}

/// Всё, что воркспейс знает о своих источниках, хранится в
/// `workspace.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// Версия формата файла.
    pub format: u32,
    /// Человеческое имя воркспейса.
    pub name: String,
    /// Директория (относительно воркспейса), где лежат результаты Parquet.
    pub data_dir: String,
    /// Подтверждённые привязки папка→сущность (режим A и подтверждённый режим B).
    #[serde(default)]
    pub bindings: Vec<Binding>,
    /// Корни, сканируемые на сущности-кандидаты (режим B). Кандидаты становятся
    /// привязками только после подтверждения пользователем.
    #[serde(default)]
    pub scan_roots: Vec<String>,
}

/// Абсолютный путь, где этот воркспейс хранит свои Parquet-датасеты.
pub fn data_dir(workspace_dir: &Path, config: &WorkspaceConfig) -> PathBuf {
    let path = PathBuf::from(&config.data_dir);
    if path.is_absolute() {
        path
    } else {
        workspace_dir.join(path)
    }
}

/// Абсолютный путь папки `schemas/` воркспейса.
pub fn schemas_dir(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("schemas")
}

/// Создаёт директорию нового воркспейса (падает, если `workspace.toml` уже есть).
pub fn create_workspace(dir: &Path, name: &str) -> crate::Result<WorkspaceConfig> {
    if dir.join("workspace.toml").exists() {
        return Err(StrataError::ProjectExists(dir.to_path_buf()));
    }
    for sub in ["schemas", "data", "plugins", "logs"] {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    let config = WorkspaceConfig {
        format: CONFIG_FORMAT,
        name: name.trim().to_string(),
        data_dir: String::from("data"),
        bindings: Vec::new(),
        scan_roots: Vec::new(),
    };
    save_config(dir, &config)?;
    Ok(config)
}

/// Открывает воркспейс: возвращает его конфиг или `None`, если папка не является воркспейсом.
pub fn open_workspace(dir: &Path) -> crate::Result<Option<WorkspaceConfig>> {
    let file = dir.join("workspace.toml");
    if !file.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&file)?;
    let config: WorkspaceConfig = toml::from_str(&text)
        .map_err(|e| StrataError::ProjectFile(format!("parse workspace.toml: {e}")))?;
    Ok(Some(config))
}

/// Сохраняет конфиг обратно в `workspace.toml` (перезапись файла целиком).
pub fn save_config(dir: &Path, config: &WorkspaceConfig) -> crate::Result<()> {
    let text = toml::to_string(config)
        .map_err(|e| StrataError::ProjectFile(format!("serialize workspace.toml: {e}")))?;
    std::fs::write(dir.join("workspace.toml"), text)?;
    Ok(())
}

/// Прямые подпапки `root` (кандидаты режима B); файлы/симлинки игнорируются.
/// Под-подпапки намеренно не возвращаются: только один уровень — правило
/// дизайна «никакой вложенности внутри папки сущности».
pub fn list_entity_candidates(root: &Path) -> crate::Result<Vec<PathBuf>> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    Ok(dirs)
}

/// Человеческое имя для папки-кандидата (её имя директории).
pub fn candidate_entity_name(folder: &Path) -> String {
    folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "entity".to_string())
}

/// Добавляет (или заменяет) подтверждённую привязку папка→сущность и сохраняет конфиг.
pub fn upsert_binding(
    dir: &Path,
    config: &mut WorkspaceConfig,
    entity: String,
    folder: PathBuf,
) -> crate::Result<()> {
    // Заменяем любую предыдущую привязку для той же сущности (побеждает последняя).
    config.bindings.retain(|b| b.entity != entity);
    config.bindings.push(Binding {
        entity,
        folder: folder.display().to_string(),
        added_utc: String::from("now"),
    });
    save_config(dir, config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("strata_ws_{}_{}_{}", std::process::id(), n, tag));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn workspace_create_open_and_binding_roundtrip() {
        let dir = temp_dir("ws");
        let config = create_workspace(&dir, "Продажи 2026").expect("create");
        assert_eq!(config.name, "Продажи 2026");
        assert!(dir.join("data").is_dir());
        assert!(dir.join("schemas").is_dir());

        let mut config = open_workspace(&dir).expect("open").expect("exists");
        assert_eq!(config.name, "Продажи 2026");
        assert_eq!(data_dir(&dir, &config), dir.join("data"));

        upsert_binding(
            &dir,
            &mut config,
            "sales".into(),
            PathBuf::from("/data/sales"),
        )
        .unwrap();
        let reopened = open_workspace(&dir).expect("open").expect("exists");
        assert_eq!(reopened.bindings.len(), 1);
        assert_eq!(reopened.bindings[0].entity, "sales");

        let err = create_workspace(&dir, "again").expect_err("duplicate fails");
        assert!(matches!(err, StrataError::ProjectExists(_)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn entity_candidates_are_direct_subfolders_only() {
        let root = temp_dir("root");
        std::fs::create_dir_all(root.join("sales/inner")).unwrap(); // более глубокая директория НЕ кандидат
        std::fs::create_dir_all(root.join("clients")).unwrap();
        std::fs::write(root.join("notes.txt"), "x").unwrap();

        let candidates = list_entity_candidates(&root).expect("list");
        let names: Vec<String> = candidates
            .iter()
            .map(|p| candidate_entity_name(p))
            .collect();
        // Только прямые подпапки: sales, clients — НЕ sales/inner и НЕ файл.
        assert_eq!(names, vec!["clients", "sales"]);
        let _ = std::fs::remove_dir_all(root);
    }
}
