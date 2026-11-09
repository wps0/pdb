use std::{
    fs::File,
    io::{Read, Seek},
    sync::Arc,
};

use pdb::storage::{
    Column, ColumnSchema, ColumnType, DataFileReader, DataFileWriter, Value,
};
use tempfile::NamedTempFile;

fn unnamed_file() -> Arc<File> {
    Arc::new(tempfile::tempfile().unwrap())
}

fn file_len(f: &Arc<File>) -> u64 {
    f.metadata().unwrap().len()
}

fn dump_from_readable<T: Read + Seek>(
    source: &mut T,
) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![];
    source.rewind()?;
    source.read_to_end(&mut buf)?;
    Ok(buf)
}

#[test]
fn test_writes_empty_file() {
    let expected = vec![
        b'I', b'S', b'D', b'B', // magic
        0, 0, 0, 0, // n_parts = 0
        0, 0, 0, 0, // n_cols = 0
    ];
    let mut tmp = unnamed_file();
    let mut writer = DataFileWriter::new(vec![], tmp.clone());

    let nwritten = writer.flush().unwrap();

    assert_eq!(nwritten, 12);
    assert_eq!(file_len(&tmp), 12);
    assert_eq!(dump_from_readable(&mut tmp).unwrap(), expected);
}

#[test]
fn test_writes_empty_file_with_columns_roundtrip() {
    let tmp = NamedTempFile::new().unwrap();
    let schema = vec![
        ColumnSchema::new(ColumnType::Bool),
        ColumnSchema::new(ColumnType::VarChar),
        ColumnSchema::new(ColumnType::Int64),
        ColumnSchema::new(ColumnType::VarChar),
    ];

    let mut writer =
        DataFileWriter::new(schema, Arc::new(tmp.reopen().unwrap()));
    let len = writer.flush().unwrap();
    let df = DataFileReader::from_path(tmp.path()).unwrap();

    assert_eq!(len, 16);
    assert_eq!(df.header.n_cols, 4);
    assert_eq!(df.header.n_parts, 0);
}

#[test]
fn test_writes_schema_bytes_in_header() {
    let expected = vec![
        b'I', b'S', b'D', b'B', // magic
        0, 0, 0, 0, // n_parts = 0
        4, 0, 0, 0, // n_cols = 4
        1, 2, 3, 3, // schema: Bool, Int64, VarChar, VarChar
    ];
    let mut tmp = unnamed_file();
    let schema = vec![
        ColumnSchema::new(ColumnType::Bool),
        ColumnSchema::new(ColumnType::Int64),
        ColumnSchema::new(ColumnType::VarChar),
        ColumnSchema::new(ColumnType::VarChar),
    ];
    let mut writer = DataFileWriter::new(schema, tmp.clone());

    writer.flush().unwrap();

    assert_eq!(dump_from_readable(&mut tmp).unwrap(), expected);
}

#[test]
fn test_second_empty_flush_writes_nothing() {
    let expected = vec![
        b'I', b'S', b'D', b'B', // magic
        0, 0, 0, 0, // n_parts = 0
        1, 0, 0, 0, // n_cols = 1
        2, // schema: Int64
    ];
    let mut tmp = unnamed_file();

    let mut writer = DataFileWriter::new(
        vec![ColumnSchema::new(ColumnType::Int64)],
        tmp.clone(),
    );
    writer.flush().unwrap();
    let nwritten = writer.flush().unwrap();

    assert_eq!(nwritten, 0);
    assert_eq!(file_len(&tmp), 13);
    assert_eq!(dump_from_readable(&mut tmp).unwrap(), expected)
}

#[test]
fn test_each_flush_appends_part_and_resets_delta() {
    #[rustfmt::skip]
    let expected = vec![
        b'I', b'S', b'D', b'B', // magic
        2, 0, 0, 0, // n_parts = 2
        2, 0, 0, 0, // n_cols = 2
        2, 1, // schema: Int64, Bool
        2, 0, 0, 0, // part 0: n_rows = 2
        2, 0, 0, 0, 0, 0, 0, 0, // part 0: column_sizes[0] = 2
        1, 0, 0, 0, 0, 0, 0, 0, // part 0: column_sizes[1] = 1
        0x0a, 0x02, // 5, 6 -> delta 5, 1 -> zigzag 10, 2
        0b0100_0000, // false, true
        1, 0, 0, 0, // part 1: n_rows = 1
        1, 0, 0, 0, 0, 0, 0, 0, // part 1: column_sizes[0] = 1
        1, 0, 0, 0, 0, 0, 0, 0, // part 1: column_sizes[1] = 1
        0x0e, // 7 -> delta 7 -> zigzag 14
        0b1000_0000, // true
    ];
    let [row0, row1, row2] = [
        vec![Value::from(5), Value::from(false)],
        vec![Value::from(6), Value::from(true)],
        vec![Value::from(7), Value::from(true)],
    ];
    let mut tmp = unnamed_file();

    let mut writer = DataFileWriter::new(
        vec![
            ColumnSchema::new(ColumnType::Int64),
            ColumnSchema::new(ColumnType::Bool),
        ],
        tmp.clone(),
    );
    writer.append_row(row0).unwrap();
    writer.append_row(row1).unwrap();
    writer.flush().unwrap();
    writer.append_row(row2).unwrap();
    writer.flush().unwrap();

    assert_eq!(dump_from_readable(&mut tmp).unwrap(), expected);
}
