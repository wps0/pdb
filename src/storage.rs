use std::cmp::min;
use std::convert::TryInto;
use std::fs::File;
use std::io::ErrorKind;
use std::io::{BufReader, BufWriter, Error, Read, Write};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::vec;

use zstd::zstd_safe::CompressionLevel;

use crate::encodings::{
    delta_decode, delta_encode, vle_decode, vle_encode, zigzag_decode,
    zigzag_encode,
};

pub(crate) static BUF_CAPACITY: usize = 1024 * 1024 * 4;

// TODO: ustawianie czy kompresowac per kolumna. domyslnie false dla bool i int, true dla varchar
#[derive(Clone, Copy)]
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

impl From<u8> for ColumnSchema {
    fn from(b: u8) -> ColumnSchema {
        let ctype = match b {
            1 => ColumnType::Bool,
            2 => ColumnType::Int64,
            3 => ColumnType::VarChar,
            v => panic!("Unsupported enum value {v}"),
        };
        ColumnSchema { ctype }
    }
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

impl DataFileHeader {
    pub fn deserialize(buf: &[u8]) -> std::io::Result<DataFileHeader> {
        if buf.len() < 16 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "corrupted header: should be at least 16B",
            ));
        }

        let (size, n_cols, n_rows) =
            Self::deserialize_sizes(buf[..16].try_into().unwrap());
        let schema = Self::deserialize_schema(&buf[16..], n_cols as usize)?;
        let column_offsets = Self::deserialize_column_offsets(
            &buf[16 + n_cols as usize..],
            n_cols as usize,
        );

        Ok(DataFileHeader {
            size,
            n_cols: n_cols as usize,
            n_rows: n_rows as usize,
            schema,
            column_offsets,
        })
    }

    fn deserialize_sizes(sizes: &[u8; 16]) -> (u64, u32, u32) {
        let size = u64::from_le_bytes(sizes[0..8].try_into().unwrap());
        let n_cols = u32::from_le_bytes(sizes[8..12].try_into().unwrap());
        let n_rows = u32::from_le_bytes(sizes[12..16].try_into().unwrap());
        (size, n_cols, n_rows)
    }

    fn serialize_sizes(&self, sizes: &mut [u8]) {
        sizes[0..8].copy_from_slice(&self.size.to_le_bytes());
        sizes[8..12].copy_from_slice(&self.n_rows.to_le_bytes());
        sizes[12..16].copy_from_slice(&self.n_cols.to_le_bytes());
    }

    fn deserialize_schema(
        buf: &[u8],
        n_cols: usize,
    ) -> std::io::Result<Vec<ColumnSchema>> {
        if buf.len() < n_cols {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "corrupted header: too short to hold schema",
            ));
        }
        let mut schema: Vec<ColumnSchema> = vec![];
        schema.reserve(n_cols);
        for i in 0..n_cols {
            schema.push(ColumnSchema::from(buf[i]));
        }
        Ok(schema)
    }

    fn serialize_schema(&self, buf: &mut [u8]) {
        for i in 0..self.n_cols {
            buf[i] = u8::from(&self.schema[i]);
        }
    }

    fn deserialize_column_offsets(buf: &[u8], n_cols: usize) -> Vec<u64> {
        let mut offsets = vec![0; n_cols];
        for i in 0..n_cols {
            offsets[i] =
                u64::from_le_bytes(buf[i * 8..(i + 1) * 8].try_into().unwrap());
        }
        offsets
    }

    pub fn deserialize_from_file(
        file: &File,
    ) -> std::io::Result<DataFileHeader> {
        let mut sizes: [u8; 16] = [0; 16];
        if let Err(e) = file.read_exact_at(&mut sizes, 0) {
            return Err(e);
        }
        let (size, n_cols, n_rows) = Self::deserialize_sizes(&sizes);

        let expected_size = n_cols + n_cols * 8;
        let mut buf = vec![0; expected_size as usize];
        if let Err(e) = file.read_exact_at(&mut buf, 16) {
            return Err(e);
        }
        let schema = Self::deserialize_schema(&buf, n_cols as usize)?;
        let column_offsets = Self::deserialize_column_offsets(
            &buf[n_cols as usize..],
            n_cols as usize,
        );

        Ok(DataFileHeader {
            size,
            n_cols: n_cols as usize,
            n_rows: n_rows as usize,
            schema,
            column_offsets,
        })
    }

    pub fn serialize(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.len() < self.disk_size() {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "buffer too short",
            ));
        }
        self.serialize_sizes(buf);
        self.serialize_schema(&mut buf[16..]);

        Ok(self.disk_size())
    }
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

impl From<&ColumnSchema> for u8 {
    fn from(schema: &ColumnSchema) -> u8 {
        schema.ctype as u8
    }
}

pub struct ColumnBytes {
    file: Arc<File>,
    pos: u64,
    end: u64,
}

impl ColumnBytes {
    pub fn new(file: Arc<File>) -> ColumnBytes {
        ColumnBytes {
            file,
            pos: 0,
            end: 0,
        }
    }
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

impl Write for ColumnBytes {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.file.write_at(buf, self.pos)?;
        self.pos += n as u64;
        self.end = self.pos;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

pub struct DataFile {
    pub header: DataFileHeader,
    inner: Arc<File>,
}

impl DataFile {
    pub fn from_path(path: &str) -> Result<DataFile, Error> {
        let file = File::open(path).unwrap();
        return Ok(DataFile {
            header: DataFileHeader::deserialize_from_file(&file)?,
            inner: Arc::new(file),
        });
    }

    fn get_column_size(&self, i: usize) -> u64 {
        assert!(self.header.column_offsets.len() > i);
        let next = if i + 1 == self.header.column_offsets.len() {
            self.header.size
        } else {
            self.header.column_offsets[i + 1]
        };
        next - self.header.column_offsets[i]
    }

    pub fn column_bytes(&self, i: usize) -> ColumnBytes {
        ColumnBytes {
            file: Arc::clone(&self.inner),
            pos: self.header.column_offsets[i],
            end: self.header.column_offsets[i] + self.get_column_size(i),
        }
    }
}

pub struct DataFileWriter {
    schema: Vec<ColumnSchema>,
    columns: Vec<Column>,
    backing_file: Arc<File>,
    eof_offset: u64,
}

pub enum Column {
    Bool(BoolColumn),
    Int64(Int64Column),
    VarChar(VarCharColumn),
}

impl DataFileWriter {
    pub fn new(
        schema: Vec<ColumnSchema>,
        backing_file: Arc<File>,
    ) -> DataFileWriter {
        let columns = schema
            .iter()
            .map(|cschema| match cschema.ctype {
                ColumnType::Bool => Column::Bool(BoolColumn::new(0)),
                ColumnType::Int64 => Column::Int64(Int64Column::new()),
                ColumnType::VarChar => Column::VarChar(VarCharColumn::new()),
                _ => unreachable!("Unsupported"),
            })
            .collect();
        DataFileWriter {
            schema,
            columns,
            backing_file,
            eof_offset: 0,
        }
    }

    fn write_file_header(&mut self) {
        let header = DataFileHeader {
            size: 0xdeadbeef,
            n_cols: self.columns.len(),
            n_rows: 0xbbadbeef,
            schema: self.schema,
            column_offsets: vec![0; self.schema.len()],
        };
        let mut buf = [0u8; 1234];
        self.backing_file.write_all(buf)
    }

    fn write_part_header(&mut self) {}

    fn write_part(&mut self) {}

    pub fn flush(&mut self) -> std::io::Result<usize> {
        if self.eof_offset == 0 {
            // Write header
            self.write_file_header();
        }
        if let Column(c) = &self.columns[0] {
            if c.len() == 0 {
                return Ok(0);
            }
        }
        self.write_part_header();
        self.write_part();
        Ok(1337);
    }
}

pub enum ColumnReader {
    Bool(BoolColumnReader),
    Int64(Int64ColumnReader),
    VarChar(VarCharColumnReader),
}

pub enum ColumnWriter {
    Bool(BoolColumnWriter),
    Int64(Int64ColumnWriter),
    VarChar(VarCharColumnWriter),
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

    pub fn append(&mut self, b: bool) {
        if self.len % 8 == 0 {
            self.bits.push(0);
        }
        self.bits[self.len / 8] |= (b as u8) << (7 - self.len % 8);
        self.len += 1;
    }
}

pub struct BoolColumnReader {
    decoder: BufReader<ColumnBytes>,
}

impl BoolColumnReader {
    pub fn new(column_bytes: ColumnBytes) -> BoolColumnReader {
        BoolColumnReader {
            decoder: BufReader::new(column_bytes),
        }
    }

    fn read_batch(&mut self, nrows: usize) -> std::io::Result<BoolColumn> {
        let mut col = BoolColumn::new(nrows);
        self.decoder.read_exact(&mut col.bits)?;
        Ok(col)
    }
}

pub struct BoolColumnWriter {
    column: BufWriter<ColumnBytes>,
}

impl BoolColumnWriter {
    pub fn new(column_bytes: ColumnBytes) -> BoolColumnWriter {
        BoolColumnWriter {
            column: BufWriter::new(column_bytes),
        }
    }

    fn write(&mut self, column_fragment: BoolColumn) -> std::io::Result<()> {
        self.column.write_all(&column_fragment.bits)
    }
}

pub struct Int64Column {
    data: Vec<i64>,
}

impl Int64Column {
    pub fn new() -> Int64Column {
        Int64Column { data: vec![] }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn append(&mut self, n: i64) {
        self.data.push(n);
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

pub struct Int64ColumnWriter {
    writer: BufWriter<ColumnBytes>,
    last_value: i64,
}

impl Int64ColumnWriter {
    pub fn new(
        column_bytes: ColumnBytes,
    ) -> std::io::Result<Int64ColumnWriter> {
        Ok(Int64ColumnWriter {
            writer: BufWriter::new(column_bytes),
            last_value: 0,
        })
    }

    pub fn write(
        &mut self,
        column_fragment: Int64Column,
    ) -> std::io::Result<()> {
        let mut buf = [0_u8; 10];
        for n in column_fragment.data {
            let n_len = vle_encode(
                zigzag_encode(delta_encode(n, self.last_value)),
                &mut buf,
            );
            self.last_value = n;
            self.writer.write_all(&buf[..n_len])?;
        }
        Ok(())
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

    pub fn append(&mut self, bytes: &mut Vec<u8>) {
        self.offsets.push(self.data.len() as u64);
        self.data.append(bytes);
    }

    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    pub fn length_of(&self, idx: usize) -> usize {
        assert!(idx < self.len());
        if idx + 1 == self.len() {
            return self.len() - self.offsets[idx] as usize;
        }
        return (self.offsets[idx + 1] - self.offsets[idx]) as usize;
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

pub struct VarCharColumnWriter {
    encoder: zstd::stream::AutoFinishEncoder<'static, BufWriter<ColumnBytes>>,
}

impl VarCharColumnWriter {
    pub fn new(
        column_bytes: ColumnBytes,
        compression_level: CompressionLevel,
    ) -> std::io::Result<VarCharColumnWriter> {
        Ok(VarCharColumnWriter {
            encoder: zstd::stream::Encoder::new(
                BufWriter::new(column_bytes),
                compression_level,
            )?
            .auto_finish(),
        })
    }

    fn write(
        &mut self,
        column_fragment: &VarCharColumn,
    ) -> std::io::Result<()> {
        for i in 0..column_fragment.len() {
            let len = column_fragment.length_of(i);
            // Beginning of the string - attach the length.
            let mut buf = [0 as u8; 10];
            let buf_n = vle_encode(len as u64, &mut buf);
            self.encoder.write_all(&mut buf[..buf_n])?;

            let offset = column_fragment.offsets[i] as usize;
            self.encoder
                .write_all(&column_fragment.data[offset..offset + len])?;
        }
        Ok(())
    }
}
