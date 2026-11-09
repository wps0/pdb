# pdb - ISBD project

## Build and run
```sh
make
make test
make clean
```

Statistics for a file (INT64 mean, BOOL binary entropy, ASCII characters in VARCHARs):
```sh
cargo run -- stats resources/pan-tadeusz.isbd
```

## File format
```
"ISDB"                      - 4 B               - magic
n_parts                     - u32               - number of parts (batches)
n_cols                      - u32               - number of columns in each part
schema                      - n_cols * u8       - column data types (1 = BOOL, 2 = INT64, 3 = VARCHAR)
part_i: part * n_parts:
  n_rows                    - u32               - number of rows in this part
  column_sizes              - n_cols * u64      - physical disk size of each column in bytes
  column 0 ... column n-1   - column_sizes[i] B - encoded column data
```

## In-memory representation
A batch is a set of columns: INT64 `Vec<i64>`, BOOL bitmap `Vec<u8>`, VARCHAR `data: Vec<u8>` + `offsets`.

## Future plans
* Rework the src in Tokio to support parallel data fetching
* Support different compression methods and encodings switchable per column by storing encoding information in ColumnSchema.