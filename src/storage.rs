use std::cmp::min;
use std::convert::TryInto;
use std::fs::File;
use std::io::{BufReader, BufWriter, Error, Read, Write};
use std::io::{ErrorKind, Seek};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;
use std::vec;

use zstd::DEFAULT_COMPRESSION_LEVEL;
use zstd::zstd_safe::CompressionLevel;

use crate::encodings::{
    delta_decode, delta_encode, vle_decode, vle_encode, zigzag_decode,
    zigzag_encode,
};

pub(crate) static BUF_CAPACITY: usize = 1024 * 1024 * 4;

// TODO: ustawianie czy kompresowac per kolumna. domyslnie false dla bool i int, true dla varchar
#[derive(Clone, Copy)]
#[repr(u8)]
pub enum ColumnType {
    Bool = 1,
    Int64 = 2,
    VarChar = 3,
    // TODO: VLE INT, jakieś encodingi
}

#[derive(Clone, Copy)]
pub struct ColumnSchema {
    ctype: ColumnType,
    // TODO: encoding, etc.
}

impl ColumnSchema {
    pub fn new(ctype: ColumnType) -> ColumnSchema {
        ColumnSchema { ctype }
    }
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
    // No. of parts. u32 in binary.
    pub n_parts: usize,
    // No. of columns. u32 in binary.
    pub n_cols: usize,
    schema: Vec<ColumnSchema>,
}

impl DataFileHeader {
    pub const MAGIC: [u8; 4] = *b"ISDB";

    fn disk_size(&self) -> usize {
        4 + 4 + 4 + self.n_cols
    }

    pub fn deserialize(buf: &[u8]) -> std::io::Result<DataFileHeader> {
        if buf.len() < 12 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "corrupted header: should be at least 12B",
            ));
        }

        let (n_parts, n_cols) =
            Self::deserialize_sizes(buf[..12].try_into().unwrap())?;
        let schema = Self::deserialize_schema(&buf[12..], n_cols as usize)?;

        Ok(DataFileHeader {
            n_parts: n_parts as usize,
            n_cols: n_cols as usize,
            schema,
        })
    }

    fn deserialize_sizes(sizes: &[u8; 12]) -> std::io::Result<(u32, u32)> {
        if sizes[0..4] != Self::MAGIC {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "not a data file: wrong magic",
            ));
        }
        let n_parts = u32::from_le_bytes(sizes[4..8].try_into().unwrap());
        let n_cols = u32::from_le_bytes(sizes[8..12].try_into().unwrap());
        Ok((n_parts, n_cols))
    }

    fn serialize_sizes(&self, sizes: &mut [u8]) {
        sizes[0..4].copy_from_slice(&Self::MAGIC);
        sizes[4..8].copy_from_slice(&(self.n_parts as u32).to_le_bytes());
        sizes[8..12].copy_from_slice(&(self.n_cols as u32).to_le_bytes());
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

    pub fn deserialize_from_file(
        file: &File,
    ) -> std::io::Result<DataFileHeader> {
        let mut sizes: [u8; 12] = [0; 12];
        file.read_exact_at(&mut sizes, 0)?;
        let (n_parts, n_cols) = Self::deserialize_sizes(&sizes)?;

        let mut buf = vec![0; n_cols as usize];
        file.read_exact_at(&mut buf, 12)?;
        let schema = Self::deserialize_schema(&buf, n_cols as usize)?;

        Ok(DataFileHeader {
            n_parts: n_parts as usize,
            n_cols: n_cols as usize,
            schema,
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
        self.serialize_schema(&mut buf[12..]);

        Ok(self.disk_size())
    }
}

pub struct DataPartHeader {
    // Length of each column in this file. u32 in binary.
    n_rows: usize,
    // Size of each column block. i-th column lies at [end of data part header]
    // + sum(column_sizes[0], ..., column_sizes[i-1]).
    column_sizes: Vec<u64>,
}

impl DataPartHeader {
    fn disk_size(n_cols: usize) -> usize {
        4 + n_cols * 8
    }

    pub fn part_size(&self) -> usize {
        Self::disk_size(self.column_sizes.len())
            + self.column_sizes.iter().sum::<u64>() as usize
    }

    pub fn deserialize(
        buf: &[u8],
        n_cols: usize,
    ) -> std::io::Result<DataPartHeader> {
        if buf.len() < Self::disk_size(n_cols) {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "corrupted part header: too short",
            ));
        }

        let n_rows = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        let mut column_sizes = vec![0; n_cols];
        for i in 0..n_cols {
            let base = 4 + i * 8;
            column_sizes[i] =
                u64::from_le_bytes(buf[base..base + 8].try_into().unwrap());
        }
        Ok(DataPartHeader {
            n_rows,
            column_sizes,
        })
    }

    pub fn deserialize_from_file(
        file: &File,
        offset: u64,
        n_cols: usize,
    ) -> std::io::Result<DataPartHeader> {
        let mut buf = vec![0; Self::disk_size(n_cols)];
        file.read_exact_at(&mut buf, offset)?;
        Self::deserialize(&buf, n_cols)
    }

    pub fn serialize(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        let size = Self::disk_size(self.column_sizes.len());
        if buf.len() < size {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "buffer too short",
            ));
        }
        buf[0..4].copy_from_slice(&(self.n_rows as u32).to_le_bytes());
        for i in 0..self.column_sizes.len() {
            let start = 4 + i * 8;
            buf[start..start + 8]
                .copy_from_slice(&self.column_sizes[i].to_le_bytes());
        }
        Ok(size)
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
    pub fn from_path(path: &Path) -> Result<DataFile, Error> {
        let file = File::open(path).unwrap();
        return Ok(DataFile {
            header: DataFileHeader::deserialize_from_file(&file)?,
            inner: Arc::new(file),
        });
    }
}

pub struct DataFileWriter {
    schema: Vec<ColumnSchema>,
    columns: Vec<Column>,
    n_parts: usize,
    backing_file: Arc<File>,
    eof_offset: usize,
    first_flush: bool,
}

pub enum Column {
    Bool(BoolColumn),
    Int64(Int64Column),
    VarChar(VarCharColumn),
}

pub trait ColumnData {
    fn count_rows(&self) -> usize;
    fn clear(&mut self);
}

impl ColumnData for Int64Column {
    fn count_rows(&self) -> usize {
        self.data.len()
    }
    fn clear(&mut self) {
        self.data = Vec::new();
    }
}

impl ColumnData for BoolColumn {
    fn count_rows(&self) -> usize {
        self.len
    }
    fn clear(&mut self) {
        self.bits = Vec::new();
        self.len = 0;
    }
}

impl ColumnData for VarCharColumn {
    fn count_rows(&self) -> usize {
        self.offsets.len()
    }
    fn clear(&mut self) {
        self.data = Vec::new();
        self.offsets = Vec::new();
    }
}

impl Column {
    fn inner(&self) -> &dyn ColumnData {
        match self {
            Column::Bool(c) => c,
            Column::Int64(c) => c,
            Column::VarChar(c) => c,
        }
    }

    fn inner_mut(&mut self) -> &mut dyn ColumnData {
        match self {
            Column::Bool(c) => c,
            Column::Int64(c) => c,
            Column::VarChar(c) => c,
        }
    }
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
        let eof_offset = 4 + 4 + 4 + schema.len();
        DataFileWriter {
            schema,
            columns,
            n_parts: 0,
            backing_file,
            eof_offset,
            first_flush: true,
        }
    }

    fn write_file_header(&mut self) -> std::io::Result<usize> {
        let header = DataFileHeader {
            n_cols: self.columns.len(),
            n_parts: self.n_parts,
            schema: self.schema.clone(),
        };
        let sz = header.disk_size();
        let mut buf = vec![0u8; sz];
        header.serialize(&mut buf)?;
        self.backing_file.write_all_at(&buf, 0)?;
        Ok(sz)
    }

    fn write_part_header(
        &mut self,
        n_rows: usize,
        column_sizes: Vec<u64>,
    ) -> std::io::Result<usize> {
        let header = DataPartHeader {
            n_rows,
            column_sizes,
        };
        let size = DataPartHeader::disk_size(header.column_sizes.len());
        let mut buf = vec![0u8; size];
        header.serialize(&mut buf)?;
        self.backing_file
            .write_all_at(&buf, self.eof_offset as u64)?;
        Ok(size)
    }

    // Assumes that eof_offset points at DataPartHeader.
    fn write_part(&mut self) -> std::io::Result<(usize, Vec<u64>)> {
        let mut offset = (self.eof_offset
            + DataPartHeader::disk_size(self.columns.len()))
            as u64;

        self.backing_file.seek(std::io::SeekFrom::Start(offset))?;

        let mut column_sizes = vec![0u64; 0];
        for column in &mut self.columns {
            match column {
                Column::Bool(c) => {
                    BoolColumnWriter::new(self.backing_file.clone()).write(c)?
                }
                Column::Int64(c) => {
                    Int64ColumnWriter::new(self.backing_file.clone())?
                        .write(c)?
                }
                Column::VarChar(c) => VarCharColumnWriter::new(
                    self.backing_file.clone(),
                    DEFAULT_COMPRESSION_LEVEL,
                )?
                .write(&c)?,
            }

            let next_offset = self.backing_file.stream_position()?;
            column_sizes.push(next_offset - offset);
            offset = next_offset;
        }

        let part_size = column_sizes.iter().sum::<u64>() as usize;
        Ok((part_size, column_sizes))
    }

    pub fn flush(&mut self) -> std::io::Result<usize> {
        let n_rows = self.columns.first().map_or(0, |c| c.inner().count_rows());
        if n_rows > 0 {
            self.n_parts += 1;
        }

        let mut file_header_size = self.write_file_header()?;

        if !self.first_flush {
            file_header_size = 0;
        }
        self.first_flush = false;
        if n_rows == 0 {
            return Ok(file_header_size);
        }

        let (part_size, column_sizes) = self.write_part()?;
        let hdr_size = self.write_part_header(n_rows, column_sizes)?;
        self.eof_offset += hdr_size + part_size;

        for c in &mut self.columns {
            c.inner_mut().clear();
        }

        Ok(file_header_size + hdr_size + part_size)
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

    pub fn read_batch(&mut self, nrows: usize) -> std::io::Result<BoolColumn> {
        let mut col = BoolColumn::new(nrows);
        self.decoder.read_exact(&mut col.bits)?;
        Ok(col)
    }
}

pub struct BoolColumnWriter {
    writer: BufWriter<Arc<File>>,
}

impl BoolColumnWriter {
    pub fn new(backend: Arc<File>) -> BoolColumnWriter {
        BoolColumnWriter {
            writer: BufWriter::new(backend),
        }
    }

    pub fn write(
        &mut self,
        column_fragment: &BoolColumn,
    ) -> std::io::Result<()> {
        self.writer.write_all(&column_fragment.bits)
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
    writer: BufWriter<Arc<File>>,
    last_value: i64,
}

impl Int64ColumnWriter {
    pub fn new(f: Arc<File>) -> std::io::Result<Int64ColumnWriter> {
        Ok(Int64ColumnWriter {
            writer: BufWriter::new(f),
            last_value: 0,
        })
    }

    pub fn write(
        &mut self,
        column_fragment: &Int64Column,
    ) -> std::io::Result<()> {
        let mut buf = [0_u8; 10];
        for &n in &column_fragment.data {
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

    pub fn read_batch(
        &mut self,
        nrows: usize,
    ) -> std::io::Result<VarCharColumn> {
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
    encoder: zstd::stream::Encoder<'static, BufWriter<Arc<File>>>,
}

impl VarCharColumnWriter {
    pub fn new(
        f: Arc<File>,
        compression_level: CompressionLevel,
    ) -> std::io::Result<VarCharColumnWriter> {
        Ok(VarCharColumnWriter {
            encoder: zstd::stream::Encoder::new(
                BufWriter::new(f),
                compression_level,
            )?,
        })
    }

    pub fn write(
        mut self,
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
        self.encoder.finish()?.flush()?;
        Ok(())
    }
}
