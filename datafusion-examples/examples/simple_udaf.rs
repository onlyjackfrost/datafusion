// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

/// In this example we will declare a single-type, single return type UDAF that computes the geometric mean.
/// The geometric mean is described here: https://en.wikipedia.org/wiki/Geometric_mean
use datafusion::arrow::{
    array::ArrayRef, array::Float32Array, datatypes::DataType, record_batch::RecordBatch,
};
use datafusion::common::cast::as_float64_array;
use datafusion::{error::Result, physical_plan::Accumulator};
use datafusion::{logical_expr::Volatility, prelude::*, scalar::ScalarValue};
use std::sync::Arc;

// create local session context with an in-memory table
fn create_context() -> Result<SessionContext> {
    use datafusion::arrow::datatypes::{Field, Schema};
    use datafusion::datasource::MemTable;
    // define a schema.
    let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Float32, false)]));

    // define data in two partitions
    let batch1 = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(Float32Array::from(vec![2.0, 4.0, 8.0]))],
    )?;
    let batch2 = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(Float32Array::from(vec![64.0]))],
    )?;

    // declare a new context. In spark API, this corresponds to a new spark SQLsession
    let ctx = SessionContext::new();

    // declare a table in memory. In spark API, this corresponds to createDataFrame(...).
    let provider = MemTable::try_new(schema, vec![vec![batch1], vec![batch2]])?;
    ctx.register_table("t", Arc::new(provider))?;
    Ok(ctx)
}

/// A UDAF has state across multiple rows, and thus we require a `struct` with that state.
#[derive(Debug)]
struct GeometricMean {
    n: u32,
    prod: f64,
}

impl GeometricMean {
    // how the struct is initialized
    pub fn new() -> Self {
        GeometricMean { n: 0, prod: 1.0 }
    }
}

// UDAFs are built using the trait `Accumulator`, that offers DataFusion the necessary functions
// to use them.
impl Accumulator for GeometricMean {
    // This function serializes our state to `ScalarValue`, which DataFusion uses
    // to pass this state between execution stages.
    // Note that this can be arbitrary data.
    fn state(&mut self) -> Result<Vec<ScalarValue>> {
        Ok(vec![
            ScalarValue::from(self.prod),
            ScalarValue::from(self.n),
        ])
    }

    // DataFusion expects this function to return the final value of this aggregator.
    // in this case, this is the formula of the geometric mean
    fn evaluate(&mut self) -> Result<ScalarValue> {
        let value = self.prod.powf(1.0 / self.n as f64);
        Ok(ScalarValue::from(value))
    }

    // DataFusion calls this function to update the accumulator's state for a batch
    // of inputs rows. In this case the product is updated with values from the first column
    // and the count is updated based on the row count
    fn update_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        if values.is_empty() {
            return Ok(());
        }
        let arr = &values[0];
        (0..arr.len()).try_for_each(|index| {
            let v = ScalarValue::try_from_array(arr, index)?;

            if let ScalarValue::Float64(Some(value)) = v {
                self.prod *= value;
                self.n += 1;
            } else {
                unreachable!("")
            }
            Ok(())
        })
    }

    // Optimization hint: this trait also supports `update_batch` and `merge_batch`,
    // that can be used to perform these operations on arrays instead of single values.
    fn merge_batch(&mut self, states: &[ArrayRef]) -> Result<()> {
        if states.is_empty() {
            return Ok(());
        }
        let arr = &states[0];
        (0..arr.len()).try_for_each(|index| {
            let v = states
                .iter()
                .map(|array| ScalarValue::try_from_array(array, index))
                .collect::<Result<Vec<_>>>()?;
            if let (ScalarValue::Float64(Some(prod)), ScalarValue::UInt32(Some(n))) =
                (&v[0], &v[1])
            {
                self.prod *= prod;
                self.n += n;
            } else {
                unreachable!("")
            }
            Ok(())
        })
    }

    fn size(&self) -> usize {
        size_of_val(self)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let ctx = create_context()?;

    // here is where we define the UDAF. We also declare its signature:
    let geometric_mean = create_udaf(
        // the name; used to represent it in plan descriptions and in the registry, to use in SQL.
        "geo_mean",
        // the input type; DataFusion guarantees that the first entry of `values` in `update` has this type.
        vec![DataType::Float64],
        // the return type; DataFusion expects this to match the type returned by `evaluate`.
        Arc::new(DataType::Float64),
        Volatility::Immutable,
        // This is the accumulator factory; DataFusion uses it to create new accumulators.
        Arc::new(|_| Ok(Box::new(GeometricMean::new()))),
        // This is the description of the state. `state()` must match the types here.
        Arc::new(vec![DataType::Float64, DataType::UInt32]),
    );
    ctx.register_udaf(geometric_mean.clone());

    let sql_df = ctx.sql("SELECT geo_mean(a) FROM t").await?;
    sql_df.show().await?;

    // get a DataFrame from the context
    // this table has 1 column `a` f32 with values {2,4,8,64}, whose geometric mean is 8.0.
    let df = ctx.table("t").await?;

    // perform the aggregation
    let df = df.aggregate(vec![], vec![geometric_mean.call(vec![col("a")])])?;

    // note that "a" is f32, not f64. DataFusion coerces it to match the UDAF's signature.

    // execute the query
    let results = df.collect().await?;

    // downcast the array to the expected type
    let result = as_float64_array(results[0].column(0))?;

    // verify that the calculation is correct
    assert!((result.value(0) - 8.0).abs() < f64::EPSILON);
    println!("The geometric mean of [2,4,8,64] is {}", result.value(0));

    Ok(())
}
