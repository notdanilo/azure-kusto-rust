use std::convert::TryInto;

use std::str::FromStr;
use std::sync::Arc;

use arrow::array::TimestampNanosecondArray;
use arrow::{
    array::{
        ArrayRef, BooleanArray, DurationNanosecondArray, Float64Array, Int32Array, Int64Array,
        StringArray,
    },
    datatypes::{DataType, Field, Schema, TimeUnit},
    record_batch::RecordBatch,
};
use azure_core::error::{ErrorKind, ResultExt};
use serde_json::Value;

use crate::error::Result;
use crate::models::ColumnType;
use crate::models::{Column, DataTable};
use crate::types::{KustoDateTime, KustoDuration};

fn convert_array_string(values: Vec<Value>) -> Result<ArrayRef> {
    let strings: Vec<Option<String>> = serde_json::from_value(Value::Array(values))?;
    let strings: Vec<Option<&str>> = strings.iter().map(Option::as_deref).collect();
    Ok(Arc::new(StringArray::from(strings)))
}

fn convert_array_datetime(values: Vec<Value>) -> Result<ArrayRef> {
    let dates: Vec<String> = serde_json::from_value(Value::Array(values))?;
    let timestamps = dates
        .into_iter()
        .map(|d| {
            KustoDateTime::from_str(&d)
                .ok()
                .map(|d| d.unix_timestamp_nanos())
                .and_then(|n| n.try_into().ok())
        })
        .collect::<Vec<Option<i64>>>();
    let dates_array = Arc::new(TimestampNanosecondArray::from(timestamps));
    Ok(dates_array)
}

fn safe_map_f64(value: Value) -> Result<Option<f64>> {
    match value {
        Value::String(val) if val == "NaN" => Ok(None),
        Value::String(val) if val == "Infinity" => Ok(Some(f64::INFINITY)),
        Value::String(val) if val == "-Infinity" => Ok(Some(-f64::INFINITY)),
        _ => Ok(serde_json::from_value(value)?),
    }
}

fn convert_array_float(values: Vec<Value>) -> Result<ArrayRef> {
    let reals: Vec<Option<f64>> = values
        .into_iter()
        .map(safe_map_f64)
        .collect::<Result<Vec<_>>>()?;
    Ok(Arc::new(Float64Array::from(reals)))
}

fn convert_array_timespan(values: Vec<Value>) -> Result<ArrayRef> {
    let strings: Vec<String> = serde_json::from_value(Value::Array(values))?;
    let durations: Vec<Option<i64>> = strings
        .iter()
        .map(|s| {
            KustoDuration::from_str(s)
                .ok()
                .and_then(|d| i64::try_from(d.whole_nanoseconds()).ok())
        })
        .collect();
    Ok(Arc::new(DurationNanosecondArray::from(durations)))
}

fn convert_array_bool(values: Vec<Value>) -> Result<ArrayRef> {
    let bools: Vec<Option<bool>> = serde_json::from_value(Value::Array(values))?;
    Ok(Arc::new(BooleanArray::from(bools)))
}

fn convert_array_i32(values: Vec<Value>) -> Result<ArrayRef> {
    let ints: Vec<Option<i32>> = serde_json::from_value(Value::Array(values))?;
    Ok(Arc::new(Int32Array::from(ints)))
}

fn convert_array_i64(values: Vec<Value>) -> Result<ArrayRef> {
    let ints: Vec<Option<i64>> = serde_json::from_value(Value::Array(values))?;
    Ok(Arc::new(Int64Array::from(ints)))
}

pub fn convert_column(data: Vec<Value>, column: &Column) -> Result<(Field, ArrayRef)> {
    let column_name = &column.column_name;
    match column.column_type {
        ColumnType::String => convert_array_string(data)
            .map(|data| (Field::new(column_name, DataType::Utf8, true), data)),
        ColumnType::Bool => convert_array_bool(data)
            .map(|data| (Field::new(column_name, DataType::Boolean, true), data)),
        ColumnType::Int => convert_array_i32(data)
            .map(|data| (Field::new(column_name, DataType::Int32, true), data)),
        ColumnType::Long => convert_array_i64(data)
            .map(|data| (Field::new(column_name, DataType::Int64, true), data)),
        ColumnType::Real => convert_array_float(data)
            .map(|data| (Field::new(column_name, DataType::Float64, true), data)),
        ColumnType::Datetime => convert_array_datetime(data).map(|data| {
            (
                Field::new(
                    column_name,
                    DataType::Timestamp(TimeUnit::Nanosecond, None),
                    true,
                ),
                data,
            )
        }),
        ColumnType::Timespan => convert_array_timespan(data).map(|data| {
            (
                Field::new(column_name, DataType::Duration(TimeUnit::Nanosecond), true),
                data,
            )
        }),
        _ => todo!(),
    }
}

pub fn convert_table(table: DataTable) -> Result<RecordBatch> {
    let mut buffer: Vec<Vec<Value>> = Vec::with_capacity(table.columns.len());
    let mut fields: Vec<Field> = Vec::with_capacity(table.columns.len());
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(table.columns.len());

    for _ in 0..table.columns.len() {
        buffer.push(Vec::with_capacity(table.rows.len()));
    }
    table.rows.into_iter().for_each(|row| match row {
        Value::Array(v) => {
            v.into_iter().enumerate().for_each(|(i, v)| {
                buffer[i].push(v);
            });
        }
        _ => unreachable!("Must be an array"),
    });

    buffer
        .into_iter()
        .zip(table.columns.into_iter())
        .map(|(data, column)| convert_column(data, &column))
        .try_for_each::<_, Result<()>>(|result| {
            let (field, data) = result?;
            fields.push(field);
            columns.push(data);
            Ok(())
        })?;

    Ok(RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)
        .context(ErrorKind::DataConversion, "Failed to create record batch")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{TableKind, V2QueryResult};
    use crate::operations::query::KustoResponseDataSetV2;
    use std::path::PathBuf;

    #[test]
    fn deserialize_column() {
        let data = r#" {
            "ColumnName": "int_col",
            "ColumnType": "int"
        } "#;

        let c: Column = serde_json::from_str(data).expect("deserialize error");
        let ref_col = Column {
            column_name: "int_col".to_string(),
            column_type: ColumnType::Int,
        };
        assert_eq!(c, ref_col);
    }

    #[test]
    fn deserialize_table() {
        let data = r#" {
            "FrameType": "DataTable",
            "TableId": 1,
            "TableName": "Deft",
            "TableKind": "PrimaryResult",
            "Columns": [
                {
                    "ColumnName": "int_col",
                    "ColumnType": "int"
                }
            ],
            "Rows": []
        } "#;

        let t: DataTable = serde_json::from_str(data).expect("deserialize error");
        let ref_tbl = DataTable {
            table_id: 1,
            table_name: "Deft".to_string(),
            table_kind: TableKind::PrimaryResult,
            columns: vec![Column {
                column_name: "int_col".to_string(),
                column_type: ColumnType::Int,
            }],
            rows: vec![],
        };
        assert_eq!(t, ref_tbl);
    }

    #[test]
    fn read_data_types() {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("tests/inputs/dataframe.json");

        let data = std::fs::read_to_string(path).expect("Failed to read file");
        let tables: Vec<V2QueryResult> =
            serde_json::from_str(&data).expect("Failed to deserialize result table");
        let response = KustoResponseDataSetV2 { results: tables };
        let record_batches = response
            .record_batches()
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("Failed to convert to record batches");

        assert!(record_batches[0].num_columns() > 0);
        assert!(record_batches[0].num_rows() > 0);
    }
}
