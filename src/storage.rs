use std::cmp::min;
use std::convert::TryInto;
use std::fs::File;
use std::io::{BufReader, Read};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::vec;

use crate::encodings::{delta_decode, vle_decode, zigzag_decode};

pub(crate) static BUF_CAPACITY: usize = 1024 * 1024 * 4;

// TODO: ustawianie czy kompresowac per kolumna. domyslnie false dla bool i int, true dla varchar

#[repr(u8)]
enum ColumnType {
    Unsupported = 0,
    Bool = 1,
    Int64 = 2,
    VarChar = 3,
    // TODO: VLE INT, jakieś encodingi
}

pub struct ColumnSchema {
    ctype: ColumnType,
    // TODO: encoding, etc.
}

pub struct DataFileHeader {
    // Total file size.
    size: u64,
    // No. of columns. u32 in binary.
    n_cols: usize,
    // Length of each column in this file. u32 in binary.
    n_rows: usize,

    schema: Vec<ColumnSchema>,
    // The first byte of the column header, relative to the beginning of file.
    column_offsets: Vec<u64>,
}

impl ColumnSchema {
    pub fn disk_size() -> usize {
        1
    }
}

impl DataFileHeader {
    pub fn disk_size(&self) -> usize {
        8 + 4 + 4 + self.n_cols * (ColumnSchema::disk_size() + 8)
    }
}

impl ColumnSchema {
    pub fn from(b: u8) -> ColumnSchema {
        let ctype = match b {
            1 => ColumnType::Bool,
            2 => ColumnType::Int64,
            3 => ColumnType::VarChar,
            v => panic!("Unsupported enum value {v}"),
        };
        return ColumnSchema { ctype };
    }
}

pub struct ColumnBytes {
    file: Arc<File>,
    pos: u64,
    end: u64,
}

impl Read for ColumnBytes {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let max_size = min(buf.len() as u64, self.end - self.pos) as usize;
        if max_size == 0 {
            return Ok(0);
        }
        let nread = self.file.read_at(&mut buf[..max_size], self.pos)?;
        self.pos += nread as u64;
        Ok(nread)
    }
}

pub struct DataFile {
    header: DataFileHeader,
    inner: File,
}

impl DataFile {
    pub fn from_path(path: &str) -> Result<DataFile, std::io::Error> {
        let file = File::open(path).unwrap();

        let mut sizes: [u8; 16] = [0; 16];
        if let Err(e) = file.read_exact_at(&mut sizes, 0) {
            return Err(e);
        }
        let size = u64::from_le_bytes(sizes[0..8].try_into().unwrap());
        let n_cols =
            u32::from_le_bytes(sizes[8..12].try_into().unwrap()) as usize;
        let n_rows =
            u32::from_le_bytes(sizes[12..16].try_into().unwrap()) as usize;

        let expected_size = n_cols + n_cols * 8;
        let mut buf = vec![0; expected_size];
        if let Err(e) = file.read_exact_at(&mut buf, 16) {
            return Err(e);
        }
        let mut schema: Vec<ColumnSchema> = vec![];
        schema.reserve(n_cols);
        for i in 0..n_cols {
            schema.push(ColumnSchema::from(buf[i]));
        }
        let mut column_offsets = vec![0; n_cols];
        for i in 0..n_cols {
            let base_buf_offset: usize = n_cols + i * 8;
            column_offsets[i] = u64::from_le_bytes(
                buf[base_buf_offset..base_buf_offset + 8]
                    .try_into()
                    .unwrap(),
            );
        }

        let header = DataFileHeader {
            size,
            n_cols,
            n_rows,
            schema,
            column_offsets,
        };
        return Ok(DataFile {
            header,
            inner: file,
        });
    }

    pub fn get_column_size(&self, i: usize) -> u64 {
        assert!(self.header.column_offsets.len() > i);
        let next = if i + 1 == self.header.column_offsets.len() {
            self.header.size
        } else {
            self.header.column_offsets[i + 1]
        };
        next - self.header.column_offsets[i]
    }
}

pub enum ColumnReader {
    Bool(BoolColumnReader),
    Int64(Int64ColumnReader),
    VarChar(VarCharColumnReader),
}

pub struct BoolColumnReader {
    decoder: zstd::stream::Decoder<'static, BufReader<ColumnBytes>>,
}

pub struct BoolColumn {
    bits: Vec<u8>,
    len: usize,
}

impl BoolColumn {
    pub fn new(initial_capacity: usize) -> BoolColumn {
        BoolColumn {
            bits: vec![0; (initial_capacity + 7) / 8],
            len: initial_capacity,
        }
    }
}

impl BoolColumnReader {
    pub fn new(column_bytes: ColumnBytes) -> std::io::Result<BoolColumnReader> {
        let decoder = zstd::stream::Decoder::new(column_bytes)?;
        Ok(BoolColumnReader { decoder })
    }

    fn read_batch(&mut self, nrows: usize) -> std::io::Result<BoolColumn> {
        let mut col = BoolColumn::new(nrows);
        self.decoder.read_exact(&mut col.bits)?;
        Ok(col)
    }
}

pub struct Int64Column {
    data: Vec<i64>,
}

impl Int64Column {
    pub fn new() -> Int64Column {
        Int64Column { data: vec![] }
    }
}

pub struct BufferedBytes<R> {
    inner: R,
    buf: Vec<u8>,
    start: usize,
    end: usize,
}

impl<R: Read> BufferedBytes<R> {
    pub fn new(inner: R, capacity: usize) -> Self {
        BufferedBytes {
            inner,
            buf: vec![0; capacity],
            start: 0,
            end: 0,
        }
    }

    pub fn refill(&mut self, upto: usize) -> std::io::Result<()> {
        if self.end - self.start >= upto {
            return Ok(());
        }
        self.buf.copy_within(self.start..self.end, 0);
        self.end -= self.start;
        self.start = 0;
        while self.end < upto {
            let n = self.inner.read(&mut self.buf[self.end..])?;
            if n == 0 {
                break;
            }
            self.end += n;
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    pub fn start(&self) -> &[u8] {
        &self.buf[self.start..self.end]
    }

    pub fn consume(&mut self, n: usize) {
        assert!(self.start + n <= self.end);
        self.start += n;
    }
}

pub struct Int64ColumnReader {
    column: BufferedBytes<ColumnBytes>,
    // Last read value for delta encoding.
    last_value: i64,
}

impl Int64ColumnReader {
    pub fn new(
        column_bytes: ColumnBytes,
    ) -> std::io::Result<Int64ColumnReader> {
        Ok(Int64ColumnReader {
            column: BufferedBytes::new(column_bytes, BUF_CAPACITY),
            last_value: 0,
        })
    }

    pub fn read_batch(&mut self, nrows: usize) -> std::io::Result<Int64Column> {
        let mut col = Int64Column::new();
        for _ in 0..nrows {
            // Heuristically refill when a VLE-encoded int may not be fully loaded in memory (hance 10 B).
            self.column.refill(10)?;
            if self.column.is_empty() {
                return Ok(col);
            }

            let (decoded, len) = vle_decode(self.column.start());
            let decoded = delta_decode(zigzag_decode(decoded), self.last_value);
            self.last_value = decoded;
            self.column.consume(len);
            col.data.push(decoded);
        }
        Ok(col)
    }
}

pub struct VarCharColumn {
    data: Vec<u8>,
    // Prefix sums.
    offsets: Vec<u64>,
}

impl VarCharColumn {
    pub fn new() -> VarCharColumn {
        VarCharColumn {
            data: vec![],
            offsets: vec![],
        }
    }
}

pub struct VarCharColumnReader {
    decoder: zstd::stream::Decoder<'static, BufReader<ColumnBytes>>,
}

impl VarCharColumnReader {
    pub fn new(
        column_bytes: ColumnBytes,
    ) -> std::io::Result<VarCharColumnReader> {
        let decoder = zstd::stream::Decoder::new(column_bytes)?;
        Ok(VarCharColumnReader { decoder })
    }

    fn read_batch(&mut self, nrows: usize) -> std::io::Result<VarCharColumn> {
        let mut col = VarCharColumn::new();
        let mut size_buf = [0; 4];
        for _ in 0..nrows {
            self.decoder.read_exact(&mut size_buf)?;
            let len = u32::from_le_bytes(size_buf);
            let offset = col.data.len();
            col.data.resize(offset + len as usize, 0);
            col.offsets.push(offset as u64);
            self.decoder.read_exact(
                &mut col.data
                    [offset as usize..(offset as u64 + len as u64) as usize],
            )?;
        }
        Ok(col)
    }
}
