use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::sync::{Arc, LazyLock};

use arrow::array::RecordBatch;
use arrow::csv::{ReaderBuilder as CsvReaderBuilder, WriterBuilder as CsvWriterBuilder};
use arrow::datatypes::SchemaRef;
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;
use arrow::json::{LineDelimitedWriter, ReaderBuilder as JsonReaderBuilder};
use bytes::Bytes;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;

use crate::{
    CancellationToken, DataFormat, DecodedStream, InterchangeError, Result, StreamOptions,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodecCapabilities {
    pub encode: bool,
    pub decode: bool,
    pub streaming: bool,
}

pub struct DecodedBatches {
    pub schema: SchemaRef,
    pub batches: Vec<RecordBatch>,
}

pub trait Codec: Send + Sync {
    fn format(&self) -> DataFormat;
    fn capabilities(&self) -> CodecCapabilities;
    fn encode_stream(
        &self,
        schema: SchemaRef,
        batches: &mut dyn Iterator<Item = Result<RecordBatch>>,
        output: &mut (dyn Write + Send),
    ) -> Result<()>;
    fn decode_stream(
        &self,
        data: Vec<u8>,
        schema: Option<SchemaRef>,
        options: StreamOptions,
        cancellation: CancellationToken,
    ) -> Result<DecodedStream>;

    fn encode(&self, schema: SchemaRef, batches: &[RecordBatch]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        let mut batches = batches.iter().cloned().map(Ok);
        self.encode_stream(schema, &mut batches, &mut output)?;
        Ok(output)
    }

    fn decode(&self, data: &[u8], schema: Option<SchemaRef>) -> Result<DecodedBatches> {
        let stream = self.decode_stream(
            data.to_vec(),
            schema,
            StreamOptions::default(),
            CancellationToken::new(),
        )?;
        let schema = stream.schema.clone();
        let batches = stream.collect_batches()?;
        Ok(DecodedBatches { schema, batches })
    }
}

#[derive(Default)]
pub struct CodecRegistry {
    codecs: HashMap<DataFormat, Arc<dyn Codec>>,
}

impl CodecRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<C: Codec + 'static>(&mut self, codec: C) {
        self.codecs.insert(codec.format(), Arc::new(codec));
    }

    pub fn get(&self, format: DataFormat) -> Result<Arc<dyn Codec>> {
        self.codecs
            .get(&format)
            .cloned()
            .ok_or_else(|| InterchangeError::CodecNotRegistered(format.as_str().to_string()))
    }
}

pub static DEFAULT_REGISTRY: LazyLock<CodecRegistry> = LazyLock::new(|| {
    let mut registry = CodecRegistry::new();
    registry.register(ArrowIpcCodec);
    registry.register(ParquetCodec);
    registry.register(CsvCodec);
    registry.register(JsonLinesCodec);
    registry
});

pub struct ArrowIpcCodec;

impl Codec for ArrowIpcCodec {
    fn format(&self) -> DataFormat {
        DataFormat::Arrow
    }

    fn capabilities(&self) -> CodecCapabilities {
        CodecCapabilities {
            encode: true,
            decode: true,
            streaming: true,
        }
    }

    fn encode_stream(
        &self,
        schema: SchemaRef,
        batches: &mut dyn Iterator<Item = Result<RecordBatch>>,
        output: &mut (dyn Write + Send),
    ) -> Result<()> {
        {
            let mut writer = StreamWriter::try_new(output, schema.as_ref())?;
            for batch in batches {
                let batch = batch?;
                if batch.schema() != schema {
                    return Err(InterchangeError::InputSchema);
                }
                writer.write(&batch)?;
            }
            writer.finish()?;
        }
        Ok(())
    }

    fn decode_stream(
        &self,
        data: Vec<u8>,
        _schema: Option<SchemaRef>,
        options: StreamOptions,
        cancellation: CancellationToken,
    ) -> Result<DecodedStream> {
        let reader = StreamReader::try_new(Cursor::new(data), None)?;
        let schema = reader.schema();
        DecodedStream::new(
            schema,
            Box::new(reader.map(|batch| batch.map_err(InterchangeError::from))),
            options,
            cancellation,
        )
    }
}

pub struct ParquetCodec;

impl Codec for ParquetCodec {
    fn format(&self) -> DataFormat {
        DataFormat::Parquet
    }

    fn capabilities(&self) -> CodecCapabilities {
        CodecCapabilities {
            encode: true,
            decode: true,
            streaming: true,
        }
    }

    fn encode_stream(
        &self,
        schema: SchemaRef,
        batches: &mut dyn Iterator<Item = Result<RecordBatch>>,
        output: &mut (dyn Write + Send),
    ) -> Result<()> {
        {
            let mut writer = ArrowWriter::try_new(output, schema.clone(), None)?;
            for batch in batches {
                let batch = batch?;
                if batch.schema() != schema {
                    return Err(InterchangeError::InputSchema);
                }
                writer.write(&batch)?;
            }
            writer.close()?;
        }
        Ok(())
    }

    fn decode_stream(
        &self,
        data: Vec<u8>,
        _schema: Option<SchemaRef>,
        options: StreamOptions,
        cancellation: CancellationToken,
    ) -> Result<DecodedStream> {
        let builder = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(data))?
            .with_batch_size(options.batch_size);
        let schema = builder.schema().clone();
        let reader = builder.build()?;
        DecodedStream::new(
            schema,
            Box::new(reader.map(|batch| batch.map_err(InterchangeError::from))),
            options,
            cancellation,
        )
    }
}

pub struct CsvCodec;

impl Codec for CsvCodec {
    fn format(&self) -> DataFormat {
        DataFormat::Csv
    }

    fn capabilities(&self) -> CodecCapabilities {
        CodecCapabilities {
            encode: true,
            decode: true,
            streaming: true,
        }
    }

    fn encode_stream(
        &self,
        schema: SchemaRef,
        batches: &mut dyn Iterator<Item = Result<RecordBatch>>,
        output: &mut (dyn Write + Send),
    ) -> Result<()> {
        {
            let mut writer = CsvWriterBuilder::new().build(output);
            for batch in batches {
                let batch = batch?;
                if batch.schema() != schema {
                    return Err(InterchangeError::InputSchema);
                }
                writer.write(&batch)?;
            }
        }
        Ok(())
    }

    fn decode_stream(
        &self,
        data: Vec<u8>,
        schema: Option<SchemaRef>,
        options: StreamOptions,
        cancellation: CancellationToken,
    ) -> Result<DecodedStream> {
        let schema = required_schema(DataFormat::Csv, schema)?;
        let reader = CsvReaderBuilder::new(schema.clone())
            .with_header(true)
            .with_batch_size(options.batch_size)
            .build(Cursor::new(data))?;
        DecodedStream::new(
            schema,
            Box::new(reader.map(|batch| batch.map_err(InterchangeError::from))),
            options,
            cancellation,
        )
    }
}

pub struct JsonLinesCodec;

impl Codec for JsonLinesCodec {
    fn format(&self) -> DataFormat {
        DataFormat::JsonLines
    }

    fn capabilities(&self) -> CodecCapabilities {
        CodecCapabilities {
            encode: true,
            decode: true,
            streaming: true,
        }
    }

    fn encode_stream(
        &self,
        schema: SchemaRef,
        batches: &mut dyn Iterator<Item = Result<RecordBatch>>,
        output: &mut (dyn Write + Send),
    ) -> Result<()> {
        {
            let mut writer = LineDelimitedWriter::new(output);
            for batch in batches {
                let batch = batch?;
                if batch.schema() != schema {
                    return Err(InterchangeError::InputSchema);
                }
                writer.write(&batch)?;
            }
            writer.finish()?;
        }
        Ok(())
    }

    fn decode_stream(
        &self,
        data: Vec<u8>,
        schema: Option<SchemaRef>,
        options: StreamOptions,
        cancellation: CancellationToken,
    ) -> Result<DecodedStream> {
        let schema = required_schema(DataFormat::JsonLines, schema)?;
        let reader = JsonReaderBuilder::new(schema.clone())
            .with_batch_size(options.batch_size)
            .build(Cursor::new(data))?;
        DecodedStream::new(
            schema,
            Box::new(reader.map(|batch| batch.map_err(InterchangeError::from))),
            options,
            cancellation,
        )
    }
}

fn required_schema(format: DataFormat, schema: Option<SchemaRef>) -> Result<SchemaRef> {
    schema.ok_or_else(|| InterchangeError::DecodeSchemaRequired(format.as_str().to_string()))
}

#[cfg(test)]
mod tests {
    use arrow::array::{Int64Array, RecordBatch, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};

    use super::*;

    fn fixture() -> (SchemaRef, Vec<RecordBatch>) {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let first = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![1, 2])),
                Arc::new(StringArray::from(vec!["ada", "grace"])),
            ],
        )
        .unwrap();
        let second = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![3])),
                Arc::new(StringArray::from(vec!["margaret"])),
            ],
        )
        .unwrap();
        (schema, vec![first, second])
    }

    #[test]
    fn default_registry_declares_arrow_ipc_capabilities() {
        let codec = DEFAULT_REGISTRY.get(DataFormat::Arrow).unwrap();

        assert_eq!(
            codec.capabilities(),
            CodecCapabilities {
                encode: true,
                decode: true,
                streaming: true,
            }
        );
    }

    #[test]
    fn registry_rejects_unregistered_formats() {
        let error = CodecRegistry::new()
            .get(DataFormat::Csv)
            .err()
            .unwrap()
            .to_string();

        assert!(error.contains("no codec is registered"));
    }

    #[test]
    fn arrow_ipc_round_trip_preserves_schema_and_batches() {
        let (schema, batches) = fixture();
        let codec = DEFAULT_REGISTRY.get(DataFormat::Arrow).unwrap();

        let encoded = codec.encode(schema.clone(), &batches).unwrap();
        let decoded = codec.decode(&encoded, None).unwrap();

        assert_eq!(decoded.schema, schema);
        assert_eq!(decoded.batches, batches);
    }

    #[test]
    fn arrow_ipc_preserves_empty_stream_schema() {
        let (schema, _) = fixture();
        let codec = DEFAULT_REGISTRY.get(DataFormat::Arrow).unwrap();

        let encoded = codec.encode(schema.clone(), &[]).unwrap();
        let decoded = codec.decode(&encoded, None).unwrap();

        assert_eq!(decoded.schema, schema);
        assert!(decoded.batches.is_empty());
    }

    #[test]
    fn parquet_round_trip_preserves_schema_and_rows() {
        let (schema, batches) = fixture();
        let codec = DEFAULT_REGISTRY.get(DataFormat::Parquet).unwrap();

        let encoded = codec.encode(schema.clone(), &batches).unwrap();
        let decoded = codec.decode(&encoded, None).unwrap();

        assert_eq!(decoded.schema, schema);
        assert_eq!(
            decoded
                .batches
                .iter()
                .map(RecordBatch::num_rows)
                .sum::<usize>(),
            3
        );
        let combined = arrow::compute::concat_batches(&schema, &decoded.batches).unwrap();
        let expected = arrow::compute::concat_batches(&schema, &batches).unwrap();
        assert_eq!(combined, expected);
    }

    #[test]
    fn parquet_preserves_empty_file_schema() {
        let (schema, _) = fixture();
        let codec = DEFAULT_REGISTRY.get(DataFormat::Parquet).unwrap();

        let encoded = codec.encode(schema.clone(), &[]).unwrap();
        let decoded = codec.decode(&encoded, None).unwrap();

        assert_eq!(decoded.schema, schema);
        assert!(decoded.batches.is_empty());
    }

    #[test]
    fn csv_round_trip_preserves_schema_and_rows() {
        let (schema, batches) = fixture();
        let codec = DEFAULT_REGISTRY.get(DataFormat::Csv).unwrap();

        let encoded = codec.encode(schema.clone(), &batches).unwrap();
        let decoded = codec.decode(&encoded, Some(schema.clone())).unwrap();

        assert!(encoded.starts_with(b"id,name\n"));
        assert_eq!(decoded.schema, schema);
        let combined = arrow::compute::concat_batches(&schema, &decoded.batches).unwrap();
        let expected = arrow::compute::concat_batches(&schema, &batches).unwrap();
        assert_eq!(combined, expected);
    }

    #[test]
    fn jsonl_round_trip_preserves_schema_and_rows() {
        let (schema, batches) = fixture();
        let codec = DEFAULT_REGISTRY.get(DataFormat::JsonLines).unwrap();

        let encoded = codec.encode(schema.clone(), &batches).unwrap();
        let decoded = codec.decode(&encoded, Some(schema.clone())).unwrap();

        assert!(encoded.starts_with(b"{\"id\":1,\"name\":\"ada\"}\n"));
        assert_eq!(decoded.schema, schema);
        let combined = arrow::compute::concat_batches(&schema, &decoded.batches).unwrap();
        let expected = arrow::compute::concat_batches(&schema, &batches).unwrap();
        assert_eq!(combined, expected);
    }

    #[test]
    fn text_decoding_requires_schema() {
        for format in [DataFormat::Csv, DataFormat::JsonLines] {
            let codec = DEFAULT_REGISTRY.get(format).unwrap();
            let error = codec.decode(b"", None).err().unwrap().to_string();

            assert!(error.contains("requires an Arrow schema"));
        }
    }
}
