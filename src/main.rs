use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use clap::{Parser, Subcommand};
use pdb::storage::{ColumnSchema, ColumnType, DataFileWriter, Value};

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
}

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
    Ok(())
}

fn main() -> ExitCode {
    let args = Args::parse();
    let result = match args.command {
        Command::Create { path, schema } => create(&path, &schema),
        Command::Stats { path } => stats(&path),
    };
    if let Err(e) = result {
        eprintln!("Error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
