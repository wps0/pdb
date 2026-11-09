use std::{
    fs::{OpenOptions},
    io::{Error, ErrorKind, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use rand::RngExt;

use clap::{Parser, Subcommand};
use pdb::storage::{
    BATCH_SIZE, Column, ColumnSchema, ColumnType, DataFileReader,
    DataFileWriter, Value,
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print statistics about a data file.
    Stats {
        /// Path to the data file.
        #[arg(required = true)]
        path: PathBuf,
    },
    /// Create a data file from CLI.
    Create {
        /// Destination where the data file should be written.
        #[arg(required = true)]
        path: PathBuf,

        /// Schema of the file, comma-separated (bool, int64, varchar).
        #[arg(short, long, required = true, value_delimiter = ',', value_parser = parse_column_type)]
        schema: Vec<ColumnType>,
    },
    /// Generate a sample data file (int64, bool, varchar, varchar).
    Generate {
        /// Destination where the data file should be written.
        #[arg(required = true)]
        path: PathBuf,

        /// Number of rows to generate.
        #[arg(long, default_value_t = 10_000)]
        rows: usize,

        /// Probability that a bool value is true.
        #[arg(long, default_value_t = 0.5)]
        true_probability: f64,
    },
    /// Reads the file between the provided incides in the given part.
    Read {
        /// Path to the data file.
        #[arg(required = true)]
        path: PathBuf,

        #[arg(long, default_value_t = 0)]
        part: usize,

        /// Starting index in the part.
        #[arg(long, default_value_t = 0)]
        from: usize,

        /// Ending index in the part.
        #[arg(long)]
        to: Option<usize>,
    },
}

// Params for generate.
const TEXT_PATH: &str = "resources/pan-tadeusz.txt";
const LINES_PER_ROW_MIN: usize = 1;
const LINES_PER_ROW_MAX: usize = 20;
const ISDB_REPEATS_MIN: usize = 1;
const ISDB_REPEATS_MAX: usize = 10;
const ROWS_PER_PART: usize = 1024;

fn parse_column_type(name: &str) -> Result<ColumnType, String> {
    match name.trim().to_lowercase().as_str() {
        "bool" => Ok(ColumnType::Bool),
        "int64" => Ok(ColumnType::Int64),
        "varchar" => Ok(ColumnType::VarChar),
        other => Err(format!("unknown column type: {other:?}")),
    }
}

fn parse_value(ctype: ColumnType, text: &str) -> Result<Value, String> {
    match ctype {
        ColumnType::Int64 => text
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .map_err(|e| format!("not an int64: {e}")),
        ColumnType::Bool => match text.trim() {
            "true" | "1" => Ok(Value::from(true)),
            "false" | "0" => Ok(Value::from(false)),
            _ => Err("expected true/false/1/0".to_string()),
        },
        ColumnType::VarChar => Ok(Value::from(text)),
    }
}

fn create(path: &Path, types: &[ColumnType]) -> std::io::Result<()> {
    let schema = types.iter().map(|t| ColumnSchema::new(*t)).collect();
    let mut writer = DataFileWriter::new(
        schema,
        Arc::new(
            OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(path)?,
        ),
    );

    let mut lines = std::io::stdin().lines();
    let mut row = Vec::with_capacity(types.len());
    let mut n_rows = 0;
    loop {
        let col = row.len();
        print!("Input[{n_rows},{col} {:?}]: ", types[col]);
        std::io::stdout().flush()?;
        let Some(line) = lines.next() else {
            break; // Ctrl+D
        };
        match parse_value(types[col], &line?) {
            Ok(value) => row.push(value),
            Err(msg) => eprintln!("{msg}"),
        }
        if row.len() == types.len() {
            writer.append_row(std::mem::take(&mut row))?;
            n_rows += 1;
        }
    }

    writer.flush()?;
    println!();
    Ok(())
}

fn stats(path: &Path) -> std::io::Result<()> {
    let file = DataFileReader::from_path(path)?;
    let n_cols = file.header.n_cols();
    let mut sums = vec![0_i128; n_cols];
    let mut trues = vec![0_u64; n_cols];
    let mut ascii = vec![[0_u64; 128]; n_cols];
    let mut n_rows = 0_u64;

    for batch in file.batches(BATCH_SIZE) {
        let batch = batch?;
        n_rows += batch.first().map_or(0, Column::len) as u64;
        for (col, column) in batch.iter().enumerate() {
            match column {
                Column::Int64(c) => sums[col] += c.sum_from(0),
                Column::Bool(c) => trues[col] += c.count_true() as u64,
                Column::VarChar(c) => {
                    for i in 0..c.len() {
                        for &b in c.get(i) {
                            if b.is_ascii() {
                                ascii[col][b as usize] += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    println!("rows: {n_rows}");
    for (col, schema) in file.header.schema().iter().enumerate() {
        match schema.ctype() {
            ColumnType::Int64 => {
                let mean = if n_rows == 0 {
                    0.0
                } else {
                    sums[col] as f64 / n_rows as f64
                };
                println!("column {col} (int64): mean = {mean}");
            }
            ColumnType::Bool => {
                let p = if n_rows == 0 {
                    0.0
                } else {
                    trues[col] as f64 / n_rows as f64
                };
                println!(
                    "column {col} (bool): binary entropy = {:.6}",
                    binary_entropy(p)
                );
            }
                        ColumnType::VarChar => {
                let total: u64 = ascii[col].iter().sum();
                println!("column {col} (varchar): ASCII characters = {total}");
                for (b, &count) in ascii[col].iter().enumerate() {
                    if count > 0 {
                        println!("  {:?}: {count}", b as u8 as char);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Binary entropy in bits of a variable that is `true` with probability `p`.
fn binary_entropy(p: f64) -> f64 {
    if p <= 0.0 || p >= 1.0 {
        return 0.0;
    }
    -(p * p.log2() + (1.0 - p) * (1.0 - p).log2())
}

fn generate(
    path: &Path,
    n_rows: usize,
    true_probability: f64,
) -> std::io::Result<()> {
    if !(0.0..=1.0).contains(&true_probability) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "true probability must be between 0 and 1",
        ));
    }
    let text = std::fs::read_to_string(TEXT_PATH)?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Err(Error::new(ErrorKind::InvalidData, "text file is empty"));
    }

    let schema = vec![
        ColumnSchema::new(ColumnType::Int64),
        ColumnSchema::new(ColumnType::Bool),
        ColumnSchema::new(ColumnType::VarChar),
        ColumnSchema::new(ColumnType::VarChar),
    ];
    let mut writer = DataFileWriter::new(
        schema,
        Arc::new(
            OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(path)?,
        ),
    );
    let mut rng = rand::rng();

    for row in 0..n_rows {
        let n_lines = rng
            .random_range(LINES_PER_ROW_MIN..=LINES_PER_ROW_MAX)
            .min(lines.len());
        let start = rng.random_range(0..=lines.len() - n_lines);
        let fragment = lines[start..start + n_lines].join("\n");
        let isdb = "isdb"
            .repeat(rng.random_range(ISDB_REPEATS_MIN..=ISDB_REPEATS_MAX));

        writer.append_row(vec![
            Value::from(fragment.len() as i64),
            Value::from(rng.random_bool(true_probability)),
            Value::from(fragment),
            Value::from(isdb),
        ])?;
        if (row + 1) % ROWS_PER_PART == 0 {
            writer.flush()?;
        }
    }
    writer.flush()?;
    Ok(())
}

fn read(
    path: &Path,
    from: usize,
    to: Option<usize>,
    part_idx: usize,
) -> std::io::Result<()> {
    let file = DataFileReader::from_path(path)?;
    if part_idx >= file.header.n_parts() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "part {part_idx} does not exist, the file has {} parts",
                file.header.n_parts()
            ),
        ));
    }
    let n_rows = file.n_rows(part_idx);
    let to = to.unwrap_or(n_rows).min(n_rows);
    let mut part_start = 0;
    for p in 0..part_idx {
        part_start += file.n_rows(p);
    }
    let mut i = from;
    for row in file.rows_offset(part_start + from) {
        if i >= to {
            break;
        }
        println!("{i}: {:?}", row?);
        i += 1;
    }
    Ok(())
}

fn main() -> ExitCode {
    let args = Args::parse();
    let result = match args.command {
        Command::Create { path, schema } => create(&path, &schema),
        Command::Stats { path } => stats(&path),
        Command::Generate {
            path,
            rows,
            true_probability,
        } => generate(&path, rows, true_probability),
        Command::Read {
            path,
            from,
            to,
            part
        } => read(&path, from, to, part),
    };
    if let Err(e) = result {
        eprintln!("Error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
