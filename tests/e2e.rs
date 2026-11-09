use std::{
    fs::File,
    io::{Read, Seek},
    sync::Arc,
};

use pdb::storage::{
    ColumnSchema, ColumnType, DataFileReader, DataFileWriter, Value,
};
use tempfile::NamedTempFile;
use rand::{RngExt, SeedableRng, rngs::StdRng};

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
    assert_eq!(df.header.n_cols(), 4);
    assert_eq!(df.header.n_parts(), 0);
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

/// Random value of the given type.
fn random_value(rng: &mut impl RngExt, ctype: ColumnType) -> Value {
    const ALPHABET: [char; 5] = ['a', 'b', '2', '3', '\0'];
    match ctype {
        ColumnType::Bool => Value::from(rng.random_bool(0.5)),
        ColumnType::Int64 => {
            Value::from(rng.random_range(0..=2_000_000) as i64 - 1_000_000)
        }
        ColumnType::VarChar => {
            let len = rng.random_range(0..=30);
            let mut s = String::new();
            for _ in 0..len {
                s.push(ALPHABET[rng.random_range(0..=ALPHABET.len() - 1)]);
            }
            Value::from(s)
        }
    }
}

#[test]
fn test_random_roundtrip() {
    let types = [ColumnType::Bool, ColumnType::Int64, ColumnType::VarChar];
    let mut rng = StdRng::seed_from_u64(4);
    for test in 0..100 {
        // given
        let n_cols = rng.random_range(1..=5);
        let mut ctypes = vec![];
        let mut schema = vec![];
        for _ in 0..n_cols {
            let ctype = types[rng.random_range(0..=types.len() - 1)];
            ctypes.push(ctype);
            schema.push(ColumnSchema::new(ctype));
        }
        let tmp = Arc::new(tempfile::tempfile().unwrap());
        let mut writer = DataFileWriter::new(schema, Arc::clone(&tmp));
        let n_rows = rng.random_range(0..=3000);
        let mut rows = vec![];
        for _ in 0..n_rows {
            let mut row = vec![];
            for &ctype in &ctypes {
                row.push(random_value(&mut rng, ctype));
            }
            writer.append_row_ref(&row).unwrap();
            rows.push(row);
            // Ends the part at random, so that parts have random sizes.
            if rng.random_bool(0.001) {
                writer.flush().unwrap();
            }
        }
        writer.flush().unwrap();

        // when
        let file =
            DataFileReader::from_file(tmp.try_clone().unwrap()).unwrap();
        let read_rows: Vec<Vec<Value>> =
            file.rows().collect::<std::io::Result<_>>().unwrap();

        // then
        assert_eq!(read_rows.len(), rows.len(), "test {test}");
        for i in 0..rows.len() {
            assert_eq!(read_rows[i], rows[i], "test {test}, row {i}");
        }
    }
}