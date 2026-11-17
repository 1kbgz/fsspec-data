use std::ops::Range;
use std::sync::Arc;

use bytes::Bytes;
use fsspec_rs::ByteRange;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use parquet::arrow::arrow_reader::ArrowReaderOptions;
use parquet::arrow::async_reader::{
    AsyncFileReader, MetadataSuffixFetch, ParquetRecordBatchStreamBuilder,
};
use parquet::errors::ParquetError;
use parquet::file::metadata::{ParquetMetaData, ParquetMetaDataReader};

use crate::{
    CancellationToken, DecodedStream, InterchangeError, ParquetCodec, Result, ScanSelection,
    StreamOptions,
};

struct RangeReader<F> {
    fetch: F,
    cancellation: CancellationToken,
}

impl<F> RangeReader<F>
where
    F: FnMut(&[ByteRange]) -> Result<Vec<Vec<u8>>> + Send,
{
    fn fetch(&mut self, ranges: &[ByteRange]) -> parquet::errors::Result<Vec<Bytes>> {
        if self.cancellation.is_cancelled() {
            return Err(ParquetError::External(Box::new(
                InterchangeError::StreamCancelled,
            )));
        }
        let data = (self.fetch)(ranges).map_err(|error| ParquetError::External(Box::new(error)))?;
        if data.len() != ranges.len() {
            return Err(ParquetError::General("range result count mismatch".into()));
        }
        Ok(data.into_iter().map(Bytes::from).collect())
    }
}

impl<F> AsyncFileReader for RangeReader<F>
where
    F: FnMut(&[ByteRange]) -> Result<Vec<Vec<u8>>> + Send,
{
    fn get_bytes(&mut self, range: Range<u64>) -> BoxFuture<'_, parquet::errors::Result<Bytes>> {
        async move { Ok(self.get_byte_ranges(vec![range]).await?.remove(0)) }.boxed()
    }

    fn get_byte_ranges(
        &mut self,
        ranges: Vec<Range<u64>>,
    ) -> BoxFuture<'_, parquet::errors::Result<Vec<Bytes>>> {
        async move {
            let bounds = ranges
                .iter()
                .map(|range| {
                    Ok(ByteRange {
                        start: Some(i64::try_from(range.start).map_err(|_| {
                            ParquetError::General("range offset exceeds i64".into())
                        })?),
                        end: Some(i64::try_from(range.end).map_err(|_| {
                            ParquetError::General("range offset exceeds i64".into())
                        })?),
                    })
                })
                .collect::<parquet::errors::Result<Vec<_>>>()?;
            let data = self.fetch(&bounds)?;
            if ranges
                .iter()
                .zip(&data)
                .any(|(range, bytes)| range.end - range.start != bytes.len() as u64)
            {
                return Err(ParquetError::General(
                    "range response length mismatch".into(),
                ));
            }
            Ok(data)
        }
        .boxed()
    }

    fn get_metadata<'a>(
        &'a mut self,
        options: Option<&'a ArrowReaderOptions>,
    ) -> BoxFuture<'a, parquet::errors::Result<Arc<ParquetMetaData>>> {
        async move {
            Ok(Arc::new(
                ParquetMetaDataReader::new()
                    .with_arrow_reader_options(options)
                    .load_via_suffix_and_finish(self)
                    .await?,
            ))
        }
        .boxed()
    }
}

impl<F> MetadataSuffixFetch for &mut RangeReader<F>
where
    F: FnMut(&[ByteRange]) -> Result<Vec<Vec<u8>>> + Send,
{
    fn fetch_suffix(&mut self, suffix: usize) -> BoxFuture<'_, parquet::errors::Result<Bytes>> {
        async move {
            let length = i64::try_from(suffix)
                .map_err(|_| ParquetError::General("footer length exceeds i64".into()))?;
            Ok(self
                .fetch(&[ByteRange {
                    start: Some(-length),
                    end: None,
                }])?
                .remove(0))
        }
        .boxed()
    }
}

fn interchange_error(error: ParquetError) -> InterchangeError {
    match error {
        ParquetError::External(source) => match source.downcast::<InterchangeError>() {
            Ok(error) => *error,
            Err(source) => ParquetError::External(source).into(),
        },
        error => error.into(),
    }
}

impl ParquetCodec {
    /// Decode selected Parquet ranges through a synchronous batch fetch callback.
    /// The callback must preserve range order and any object validator across calls.
    /// Footer requests use negative starts and unbounded ends. Data is fetched lazily
    /// one row group at a time; request concurrency belongs to the callback.
    pub fn decode_ranges<F>(
        &self,
        fetch: F,
        selection: &ScanSelection,
        options: StreamOptions,
        cancellation: CancellationToken,
    ) -> Result<DecodedStream>
    where
        F: FnMut(&[ByteRange]) -> Result<Vec<Vec<u8>>> + Send + Unpin + 'static,
    {
        if options.batch_size == 0 {
            return Err(InterchangeError::InvalidBatchSize);
        }
        let reader = RangeReader {
            fetch,
            cancellation: cancellation.clone(),
        };
        // Parquet only yields for reader I/O; our callbacks complete synchronously.
        let builder = ParquetRecordBatchStreamBuilder::new(reader)
            .now_or_never()
            .expect("synchronous range reads complete without yielding")
            .map_err(interchange_error)?;
        let schema = builder.schema().clone();
        let builder = crate::codec::select_parquet(builder, selection, options)?;
        let mut reader = builder.build().map_err(interchange_error)?;
        let schema = Arc::new(
            reader
                .schema()
                .as_ref()
                .clone()
                .with_metadata(schema.metadata().clone()),
        );
        let batch_schema = schema.clone();
        let batches = std::iter::from_fn(move || {
            reader
                .next()
                .now_or_never()
                .expect("synchronous range reads complete without yielding")
                .map(|batch| {
                    Ok(batch
                        .map_err(interchange_error)?
                        .with_schema(batch_schema.clone())?)
                })
        });
        let stream = DecodedStream::new(
            schema,
            Box::new(batches),
            StreamOptions {
                byte_limit: None,
                ..options
            },
            cancellation.clone(),
        )?;
        match &selection.columns {
            Some(columns) => crate::stream::project_columns(stream, columns, options, cancellation),
            None => DecodedStream::new(
                stream.schema.clone(),
                Box::new(stream),
                options,
                cancellation,
            ),
        }
    }
}
