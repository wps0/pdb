use std::{io::Read, sync::Arc};

use pdb::storage::{ColumnSchema, ColumnType, DataFile, DataFileWriter};
use tempfile::NamedTempFile;

fn dump_from_readable<T: Read>(source: &mut T) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![];
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
    let mut tmp = Arc::new(tempfile::tempfile().unwrap());
    let mut writer = DataFileWriter::new(vec![], tmp.clone());

    let nwritten = writer.flush().unwrap();

    assert_eq!(nwritten, 12);
    assert_eq!(tmp.metadata().unwrap().len(), 12);
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
    let df = DataFile::from_path(tmp.path()).unwrap();

    assert_eq!(len, 16);
    assert_eq!(df.header.n_cols, 4);
    assert_eq!(df.header.n_parts, 0);
}
