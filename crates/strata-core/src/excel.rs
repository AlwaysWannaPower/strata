//! # Поддержка Excel (XLSX/XLS) — движок (M2)
//!
//! Rust-биндинги Polars **не** читают Excel нативно, поэтому модуль использует
//! `calamine` (чисто-Rust читалку книг), а затем прогоняет строки через тот же
//! UTF-8 CSV-пайплайн, что и остальной движок — так мы бесплатно получаем
//! инференс типов Polars, опцию заголовка и строгое декодирование.
//!
//! Преобразование такое: книга → (первый) лист → CSV-текст в памяти →
//! [`crate::read_text_from_buffer`]. Цена — один буфер размером с лист на
//! чтение; для размеров staging-слоя это приемлемо, и то же правило
//! «предпросмотр читает ограниченный префикс» работает через `max_rows`
//! (calamine перестаёт перебирать строки рано, так что мы никогда не грузим
//! гигантский лист целиком ради показа 50 строк).
//!
//! ## Fidelity note (honest)
//! *Форматирование* данных (цвета, числовые форматы, шрифты) намеренно теряется:
//! мы переносим значения, а не представление. Числа и строки сохраняют свои типы;
//! даты приходят как серийные числа Excel или строки — зависит от файла.

use calamine::{Data, Reader, Xlsx};
use polars::prelude::DataFrame;
use std::path::Path;

use crate::read_text_from_buffer;

/// Читает первый лист книги Excel в `DataFrame` Polars.
///
/// * `has_header` — первая строка это имена колонок (та же опция, что и для CSV);
/// * `max_rows: None` читает весь лист (staging), `Some(n)` останавливается рано
///   (предпросмотр — дешёвый даже на огромных листах).
///
/// В Excel нет единого «разделителя», поэтому разделители/кодировки читалки тут
/// не применяются; значения уже декодированы в Unicode средствами `calamine`.
pub fn read_excel_frame(
    path: &Path,
    has_header: bool,
    max_rows: Option<usize>,
) -> crate::Result<DataFrame> {
    let mut workbook: Xlsx<_> = calamine::open_workbook(path).map_err(|err| {
        crate::StrataError::Encoding(format!("cannot open workbook {}: {err}", path.display()))
    })?;

    // По умолчанию первый лист (выбор листа — отдельный милестон).
    let sheet_name = workbook
        .sheet_names()
        .into_iter()
        .next()
        .ok_or_else(|| crate::StrataError::Encoding("workbook has no sheets".into()))?;
    let range = workbook
        .worksheet_range(&sheet_name)
        .map_err(|err| crate::StrataError::Encoding(format!("read sheet '{sheet_name}': {err}")))?;

    // Строки как ячейки → пишем их как крошечный CSV в памяти, который наша
    // читалка текста разберёт с инференсом типов Polars. Кавычки/запятые/переводы
    // строк экранируются, чтобы текст ячейки не ломал форму CSV.
    let mut csv = String::with_capacity(64 * 1024);
    let mut emitted_rows = 0usize;
    for (row_index, row) in range.rows().enumerate() {
        if let Some(limit) = max_rows {
            // Строку заголовка сохраняем, а как только выдали достаточно строк данных — стоп.
            let data_emitted = if has_header {
                emitted_rows.saturating_sub(1)
            } else {
                emitted_rows
            };
            if row_index > 0 && data_emitted >= limit {
                break;
            }
        }
        emit_csv_row(&mut csv, row)?;
        emitted_rows += 1;
    }
    // Файлу из одного заголовка всё равно нужен завершающий перевод строки для CSV-парсера.
    if !csv.ends_with('\n') {
        csv.push('\n');
    }

    // Переиспользуем жадный CSV-разбор движка (разделитель — запятая, UTF-8 по построению).
    read_text_from_buffer(csv, ',', has_header, max_rows)
}

/// Добавляет одну строку листа в CSV в памяти с правильным квотированием.
fn emit_csv_row(out: &mut String, row: &[Data]) -> crate::Result<()> {
    for (index, cell) in row.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let text = cell_text(cell);
        if text.contains([',', '"', '\n', '\r']) {
            out.push('"');
            for ch in text.chars() {
                if ch == '"' {
                    out.push('"'); // экранируем кавычки внутри значения (конвенция CSV)
                }
                out.push(ch);
            }
            out.push('"');
        } else {
            out.push_str(&text);
        }
    }
    out.push('\n');
    Ok(())
}

/// Отрисовывает одну ячейку Excel в текст для прогона через CSV.
fn cell_text(cell: &Data) -> String {
    match cell {
        Data::Int(value) => value.to_string(),
        Data::Float(value) => format_float(*value),
        Data::String(value) => value.clone(),
        Data::Bool(value) => value.to_string(),
        Data::DateTime(value) => format!("{value}"), // серийная дата Excel
        Data::DateTimeIso(value) => value.clone(),
        Data::DurationIso(value) => value.clone(),
        Data::Error(error) => format!("ERROR:{error}"),
        Data::Empty => String::new(),
    }
}

/// Держит float читаемым (`12.5`, а не `12.5000000000001`) — это забота только
/// уровня отображения; Polars всё равно типизирует колонку как f64.
fn format_float(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        let text = format!("{value}");
        text
    }
}

#[cfg(test)]
mod tests {
    // Тесты используют только публичный API ядра (crate::…) и писатель xlsx,
    // поэтому `use super::*` здесь не нужен — держим импорты точными.
    use crate::preview_source_with;
    use rust_xlsxwriter::Workbook;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_xlsx(tag: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "strata_xlsx_{}_{}_{}.xlsx",
            std::process::id(),
            n,
            tag
        ))
    }

    fn write_sample(path: &std::path::Path, rows: &[(&str, f64)]) {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_worksheet();
        sheet.write_string(0, 0, "date").unwrap();
        sheet.write_string(0, 1, "amount").unwrap();
        for (i, &(date, amount)) in rows.iter().enumerate() {
            let row = (i + 1) as u32;
            sheet.write_string(row, 0, date).unwrap();
            sheet.write_number(row, 1, amount).unwrap();
        }
        workbook.save(path).unwrap();
    }

    #[test]
    fn excel_file_is_read_into_typed_columns() {
        let path = temp_xlsx("basic");
        write_sample(&path, &[("2026-01-05", 12.5), ("2026-01-06", 7.25)]);

        let options = crate::ReaderOptions {
            has_header: true,
            ..crate::ReaderOptions::default()
        };
        let preview = preview_source_with(&path, 50, options).expect("preview xlsx");
        assert_eq!(preview.source.kind, crate::SourceKind::Excel);
        assert_eq!(preview.columns.len(), 2);
        assert_eq!(preview.columns[0].name, "date");
        assert_eq!(preview.columns[0].dtype, "str");
        assert_eq!(preview.columns[1].dtype, "f64");
        assert_eq!(preview.rows.len(), 2);
        assert_eq!(preview.rows[0][0], "2026-01-05");
        assert_eq!(preview.rows[0][1], "12.5");

        // Staging Excel-файла тоже идёт через общий пайплайн.
        let parquet = path.with_extension("parquet");
        let report = crate::source_to_parquet(&path, &parquet).expect("stage xlsx");
        assert_eq!(report.rows, 2);
        assert_eq!(report.columns, 2);
        assert_eq!(report.source.kind, crate::SourceKind::Excel);

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(parquet);
    }
}
