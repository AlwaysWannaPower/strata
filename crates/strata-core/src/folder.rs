//! # Источники-папки: директория = один сырой датасет (M1a)
//!
//! Продуктовая идея (см. `ТЗ.md` §2-§6 и `PLAN.md` M1) в том, что **папка со
//! множеством файлов** становится одним логическим `Source`, а после staging —
//! одним **Parquet-датасетом**: директорией из `part-*.parquet`, а не одним
//! гигантским файлом.
//!
//! Модуль добавляет эту «папочную» историю поверх staging одного файла из
//! [`crate::source_to_parquet`]:
//!
//! * [`scan_folder`] — перечисляет содержимое папки (имена, размеры, примерный
//!   вид) для UI, *не* читая данные;
//! * [`folder_to_parquet`] — стейджит каждый файл папки в свою часть
//!   `NNNN-<name>.parquet` в директории назначения (каждая часть — достоверный
//!   перенос сырых данных, см. философию staging в документации крейта);
//!   ошибки по отдельным файлам собираются в отчёт, но не фатальны — как
//!   настоящий ETL-прогон, который продолжается, когда один файл битый;
//! * [`preview_parts`] — читает директорию датасета обратно для общего
//!   предпросмотра (объединяет только части, чьи колонки совпадают с первой).
//!
//! Партиционирование по бизнес-ключам (`year=…/month=…`) и общий шаг
//! схемы/валидации появятся в следующем срезе M1; здесь каждый исходный файл
//! просто становится одним файлом-частью.

use std::fs;
use std::path::Path;

use crate::{
    ColumnInfo, Preview, Result, SourceInfo, SourceKind, preview_source, source_to_parquet,
};

/// Один файл, найденный внутри источника-папки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    /// Имя файла (не полный путь) — то, что показывает UI.
    pub name: String,
    /// Размер файла в байтах.
    pub size_bytes: u64,
    /// Примерная метка вида для листинга: `CSV`, `TSV`, `Parquet`, …
    /// (Определяется дёшево по расширению + magic-байтам, *до* настоящего
    /// разбора; авторитетное определение по файлу происходит при staging.)
    pub kind: String,
}

/// Результат [`scan_folder`]: всё, что нужно UI, чтобы показать источник-папку.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderScan {
    /// Файлы папки, отсортированные по имени (детерминированный порядок для UI).
    pub files: Vec<FileMeta>,
    /// Сумма размеров файлов (байты) — та самая карточка «12.4 GB» из `ТЗ.md` §5.
    pub total_size_bytes: u64,
}

/// Один успешно застейдженный файл внутри директории датасета.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedFile {
    /// Имя исходного файла, из которого получилась эта часть.
    pub name: String,
    /// Абсолютный путь записанной части `NNNN-….parquet`.
    pub part_path: String,
    /// Сколько строк данных застейджено из этого файла.
    pub rows: u64,
    /// Сколько колонок застейджено из этого файла.
    pub columns: usize,
}

/// Результат [`folder_to_parquet`] — отчёт о ETL-прогоне.
///
/// Успешные части попадают в [`FolderReport::staged`]; файлы, которые не
/// удалось прочитать, перечислены в [`FolderReport::skipped`] с текстом ошибки,
/// чтобы пользователь видел, *что* пропущено и *почему* (это идея «проблем» из
/// `ТЗ.md` §8 в самом простом сыром виде).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderReport {
    /// Успешно застейдженные части.
    pub staged: Vec<StagedFile>,
    /// Пары `(имя файла, ошибка)` для файлов, которые не удалось застейджить.
    pub skipped: Vec<(String, String)>,
    /// Сумма строк по всем застейдженным частям.
    pub total_rows: u64,
    /// Директория назначения, где лежат части.
    pub dest_dir: String,
}

/// Смотрит на папку (нерекурсивно, в M1a только верхний уровень) и описывает
/// её файлы для UI.
///
/// # Errors
/// Возвращает [`crate::StrataError::Io`], если папку вообще нельзя прочитать.
pub fn scan_folder(dir: &Path) -> Result<FolderScan> {
    let mut files = Vec::new();
    let mut total_size_bytes = 0u64;

    let mut entries: Vec<_> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .collect();
    // Детерминированный листинг: сортировка по имени держит UI стабильным между
    // прогонами и даёт нумерации частей (`NNNN-…`) предсказуемый порядок.
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // скрытые файлы (вроде .DS_Store) — не данные
        }
        let path = entry.path();
        let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        total_size_bytes += size_bytes;
        files.push(FileMeta {
            name,
            size_bytes,
            kind: cheap_kind_label(&path),
        });
    }

    Ok(FolderScan {
        files,
        total_size_bytes,
    })
}

/// Дешёвая метка вида без разбора: у Parquet есть magic-префикс `PAR1`, для
/// текстовых видов берётся расширение файла.
fn cheap_kind_label(path: &Path) -> String {
    if let Ok(mut file) = fs::File::open(path) {
        use std::io::Read;
        let mut magic = [0u8; 4];
        if file.read_exact(&mut magic).is_ok() && &magic == b"PAR1" {
            return "Parquet".to_string();
        }
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "csv" => "CSV".to_string(),
        "tsv" => "TSV".to_string(),
        "txt" => "Text".to_string(),
        "parquet" | "pq" => "Parquet".to_string(),
        "xlsx" | "xls" => "Excel".to_string(),
        _ => "Other".to_string(),
    }
}

/// Стейджит каждый файл `src_dir` в директорию датасета `dest_dir`.
///
/// Каждый исходный файл становится одной частью `NNNN-<stem>.parquet`. Файлы,
/// которые не удалось застейджить, пропускаются (ошибка сохраняется в отчёте),
/// чтобы один битый файл не останавливал всю папку — намеренное ETL-подобное
/// поведение.
///
/// # Errors
/// Ошибка возвращается только если не удалось создать директорию назначения.
/// Проблемы по отдельным файлам сообщаются через [`FolderReport::skipped`].
pub fn folder_to_parquet(src_dir: &Path, dest_dir: &Path) -> Result<FolderReport> {
    fs::create_dir_all(dest_dir)?;

    let scan = scan_folder(src_dir)?;
    let mut staged = Vec::new();
    let mut skipped = Vec::new();
    let mut total_rows = 0u64;

    for (index, meta) in scan.files.iter().enumerate() {
        // Стейджатся только файлы, в которых есть смысл. Всё неизвестного вида
        // (случайные бинарники, системный мусор) *сообщается*, а не угадывается —
        // угадывание нарушило бы обещание staging «достоверный перенос».
        if meta.kind == "Other" {
            skipped.push((meta.name.clone(), String::from("unsupported file kind")));
            continue;
        }

        let source_path = src_dir.join(&meta.name);
        let stem = source_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("file{index}"));
        // Номерной префикс делает части уникальными, даже когда два исходных
        // файла имеют один стем ("sales.csv" рядом с "sales.tsv").
        let part_path = dest_dir.join(format!("{index:04}-{stem}.parquet"));

        match source_to_parquet(&source_path, &part_path) {
            Ok(report) => {
                total_rows += report.rows;
                staged.push(StagedFile {
                    name: meta.name.clone(),
                    part_path: part_path.display().to_string(),
                    rows: report.rows,
                    columns: report.columns,
                });
            }
            Err(err) => skipped.push((meta.name.clone(), err.to_string())),
        }
    }

    Ok(FolderReport {
        staged,
        skipped,
        total_rows,
        dest_dir: dest_dir.display().to_string(),
    })
}

/// Стейджит каждый файл `src_dir` в **партиционированный** датасет под
/// `dest_root`, группируя строки по уникальным строковым значениям
/// `partition_column` (директории в стиле Hive `column=value/`).
///
/// Та же ETL-семантика, что у [`folder_to_parquet`]: хорошие файлы → части,
/// неизвестные виды / упавшие файлы → [`FolderReport::skipped`]. Каждый исходный
/// файл может породить несколько директорий партиций.
pub fn folder_to_parquet_partitioned(
    src_dir: &Path,
    dest_root: &Path,
    partition_column: &str,
) -> Result<FolderReport> {
    fs::create_dir_all(dest_root)?;
    let scan = scan_folder(src_dir)?;

    let mut staged = Vec::new();
    let mut skipped = Vec::new();
    let mut total_rows = 0u64;

    for meta in scan.files.iter() {
        if meta.kind == "Other" {
            skipped.push((meta.name.clone(), String::from("unsupported file kind")));
            continue;
        }
        let source_path = src_dir.join(&meta.name);
        match crate::source_to_parquet_partitioned(
            &source_path,
            dest_root,
            partition_column,
            crate::ReaderOptions::default(),
        ) {
            Ok(report) => {
                total_rows += report.rows;
                staged.push(StagedFile {
                    name: meta.name.clone(),
                    part_path: report.parquet_path.clone(),
                    rows: report.rows,
                    columns: report.columns,
                });
            }
            Err(err) => skipped.push((meta.name.clone(), err.to_string())),
        }
    }

    Ok(FolderReport {
        staged,
        skipped,
        total_rows,
        dest_dir: dest_root.display().to_string(),
    })
}

/// Один файл-часть Parquet внутри директории датасета (на любом уровне вложенности).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatasetPart {
    /// Путь относительно корня датасета, например `city=Moscow/part-0000-x.parquet`.
    pub rel_path: String,
    /// Размер файла в байтах.
    pub size_bytes: u64,
}

/// Перечисляет все части Parquet под `dataset_dir` рекурсивно
/// (в партиционированных датасетах части лежат внутри папок `column=value/`).
///
/// Отсортировано по относительному пути для стабильного листинга. Неизвестные
/// папки/файлы, кроме `*.parquet`, игнорируются.
pub fn list_parts(dataset_dir: &Path) -> Result<Vec<DatasetPart>> {
    let mut found = Vec::new();
    collect_parts(dataset_dir, dataset_dir, &mut found)?;
    found.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(found)
}

fn collect_parts(root: &Path, dir: &Path, found: &mut Vec<DatasetPart>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_parts(root, &path, found)?;
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("parquet"))
        {
            let rel_path = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            found.push(DatasetPart {
                rel_path,
                size_bytes,
            });
        }
    }
    Ok(())
}

/// Общий предпросмотр директории датасета (все части `*.parquet` на любой
/// глубине вложенности — и простые файлы-части, и партиционированные раскладки).
///
/// Части предпросматриваются в отсортированном порядке. Часть, колонки которой
/// отличаются от первой, пропускается (несоответствие схеме — забота
/// *валидации*, M2+, а не то, вокруг чего сырой предпросмотр должен молча
/// угадывать).
///
/// # Errors
/// Возвращает [`crate::StrataError::Io`], если директорию нельзя прочитать.
pub fn preview_parts(dataset_dir: &Path, max_rows: usize) -> Result<Preview> {
    let parts = list_parts(dataset_dir)?;

    let mut columns: Option<Vec<ColumnInfo>> = None;
    let mut rows = Vec::new();

    for part in parts {
        if rows.len() >= max_rows {
            break;
        }
        let part_path = dataset_dir.join(&part.rel_path);
        let Ok(preview) = preview_source(&part_path, max_rows - rows.len()) else {
            continue; // нечитаемая часть: здесь молча пропускаем; staging бы о ней сообщил
        };
        match &columns {
            None => columns = Some(preview.columns),
            // Объединяем, только когда часть совпадает по форме с первой частью.
            Some(expected) if *expected != preview.columns => continue,
            Some(_) => {}
        }
        rows.extend(preview.rows);
    }

    Ok(Preview {
        columns: columns.unwrap_or_default(),
        rows,
        // Происхождение бинарного файла; количество частей UI добавит из листинга.
        source: SourceInfo {
            kind: SourceKind::Parquet,
            encoding: String::from("— (binary)"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Уникальные временные директории, чтобы параллельные тесты не сталкивались.
    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "strata_folder_{}_{}_{}",
            std::process::id(),
            n,
            tag
        ));
        let _ = fs::remove_dir_all(&dir); // убираем залежавшиеся остатки
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    const SAMPLE_CSV: &str = "id,date,amount,customer\n\
1,2026-01-02,120.50,Acme Corp\n\
2,2026-01-02,75.00,Globex\n\
3,2026-01-03,240.00,Initech\n";

    #[test]
    fn scan_folder_lists_files_with_kind_and_size() {
        let dir = temp_dir("scan");
        let mut a = fs::File::create(dir.join("sales_a.csv")).unwrap();
        a.write_all(SAMPLE_CSV.as_bytes()).unwrap();
        let mut b = fs::File::create(dir.join("sales_b.tsv")).unwrap();
        b.write_all(b"id\tname\n1\tAlice\n").unwrap();
        fs::write(dir.join("notes.txt"), b"not data at all").unwrap();

        let scan = scan_folder(&dir).expect("scan succeeds");
        assert_eq!(scan.files.len(), 3);
        // Сортировка по имени: "notes.txt" < "sales_a.csv" < "sales_b.tsv".
        assert_eq!(scan.files[0].kind, "Text");
        assert_eq!(scan.files[1].kind, "CSV");
        assert_eq!(scan.files[2].kind, "TSV");
        assert!(scan.total_size_bytes > 0);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn folder_staging_keeps_good_files_and_reports_bad_ones() {
        let src = temp_dir("stage_src");
        let mut a = fs::File::create(src.join("sales_a.csv")).unwrap();
        a.write_all(SAMPLE_CSV.as_bytes()).unwrap();
        let mut b = fs::File::create(src.join("sales_b.csv")).unwrap();
        b.write_all(
            "id,date,amount,customer\n4,2026-02-01,10.00,Wayne Ent\n\
             5,2026-02-02,20.00,Wayne Ent\n6,2026-02-03,30.00,Wayne Ent\n"
                .as_bytes(),
        )
        .unwrap();
        // Файл, который нельзя разобрать как данные: пропускается, но это не фатально.
        fs::write(src.join("broken.dat"), b"\x00\x01\x02not a table").unwrap();

        let dest = temp_dir("stage_dst");
        let report = folder_to_parquet(&src, &dest).expect("staging succeeds");

        assert_eq!(report.staged.len(), 2, "two good files staged");
        assert_eq!(report.skipped.len(), 1, "broken file reported as skipped");
        assert_eq!(report.total_rows, 6, "3 + 3 rows across both parts");
        assert_eq!(report.skipped[0].0, "broken.dat");

        // Каждый застейдженный файл породил настоящую .parquet-часть на диске.
        let parts: Vec<String> = fs::read_dir(&dest)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".parquet"))
            .collect();
        assert_eq!(parts.len(), 2);

        // А общий предпросмотр возвращает все строки с теми же колонками.
        let preview = preview_parts(&dest, 100).expect("preview parts");
        assert_eq!(preview.columns.len(), 4);
        assert_eq!(preview.rows.len(), 6);

        let _ = fs::remove_dir_all(src);
        let _ = fs::remove_dir_all(dest);
    }

    // ------------------------------------------------------------------
    // M1b (шаг 2): партиционированная запись датасета + рекурсивный листинг датасета
    // ------------------------------------------------------------------

    #[test]
    fn partitioned_staging_writes_hive_folders_per_value() {
        let csv = temp_dir("part_src").join("sales.csv");
        fs::create_dir_all(csv.parent().unwrap()).unwrap();
        fs::write(&csv, "id,city\n1,Moscow\n2,Kazan\n3,Moscow\n4,Kazan\n").unwrap();
        let dest = temp_dir("part_dst");

        let report = crate::source_to_parquet_partitioned(
            &csv,
            &dest,
            "city",
            crate::ReaderOptions::default(),
        )
        .expect("partitioned staging succeeds");

        assert_eq!(report.rows, 4);
        assert_eq!(report.columns, 2);
        assert_eq!(report.partitions, 2, "Moscow + Kazan");

        // Hive-папки существуют, и в каждой лежит файл-часть.
        assert!(dest.join("city=Moscow").is_dir());
        assert!(dest.join("city=Kazan").is_dir());
        let parts = list_parts(&dest).expect("list parts");
        assert_eq!(parts.len(), 2);

        // Рекурсивный общий предпросмотр снова видит все строки.
        let preview = preview_parts(&dest, 100).expect("preview partitioned");
        assert_eq!(preview.columns.len(), 2);
        assert_eq!(preview.rows.len(), 4);

        let _ = fs::remove_dir_all(csv.parent().unwrap());
        let _ = fs::remove_dir_all(dest);
    }

    #[test]
    fn partitioned_folder_staging_reports_rows_and_skips() {
        let src = temp_dir("pfsrc");
        fs::write(src.join("a.csv"), "id,city\n1,Moscow\n2,Kazan\n").unwrap();
        fs::write(src.join("b.csv"), "id,city\n3,Moscow\n").unwrap();
        fs::write(src.join("junk.dat"), b"\x00\x01").unwrap();
        let dest = temp_dir("pfdst");

        let report = folder_to_parquet_partitioned(&src, &dest, "city").expect("folder stage");
        assert_eq!(report.staged.len(), 2);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].0, "junk.dat");
        assert_eq!(report.total_rows, 3);

        // Оба города стали папками партиций, части находятся рекурсивно.
        assert!(dest.join("city=Moscow").is_dir());
        assert!(dest.join("city=Kazan").is_dir());
        assert_eq!(list_parts(&dest).expect("list").len(), 3); // 2 части по Москве + 1 часть по Казани

        let _ = fs::remove_dir_all(src);
        let _ = fs::remove_dir_all(dest);
    }

    #[test]
    fn partitioning_requires_a_string_column() {
        let csv = temp_dir("badpart").join("nums.csv");
        fs::create_dir_all(csv.parent().unwrap()).unwrap();
        fs::write(&csv, "id,amount\n1,12.5\n2,7.25\n").unwrap();
        let dest = temp_dir("badpart_dst");

        let err = crate::source_to_parquet_partitioned(
            &csv,
            &dest,
            "amount", // Float64 — в сыром слое не партиционируется
            crate::ReaderOptions::default(),
        )
        .expect_err("numeric partition column must fail");

        let crate::StrataError::PartitionColumn(name) = err else {
            panic!("expected PartitionColumn error, got {err:?}");
        };
        assert_eq!(name, "amount");

        let _ = fs::remove_dir_all(csv.parent().unwrap());
        let _ = fs::remove_dir_all(dest);
    }
}
