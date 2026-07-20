use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::sync::{Arc, LazyLock};

use arrow::array::RecordBatch;
use arrow::csv::{
    ReaderBuilder as CsvReaderBuilder, Writer as CsvWriter, WriterBuilder as CsvWriterBuilder,
};
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

pub trait CodecWriter: Send {
    fn write_batch(&mut self, batch: &RecordBatch) -> Result<()>;
    fn finish(self: Box<Self>) -> Result<()>;
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

    fn start_writer<'a>(
        &self,
        _schema: SchemaRef,
        _output: &'a mut (dyn Write + Send),
    ) -> Result<Box<dyn CodecWriter + 'a>> {
        Err(InterchangeError::CodecWriterNotSupported(
            self.format().as_str().to_string(),
        ))
    }

    fn start_owned_writer(
        &self,
        _schema: SchemaRef,
        _output: Box<dyn Write + Send>,
    ) -> Result<Box<dyn CodecWriter>> {
        Err(InterchangeError::CodecWriterNotSupported(
            self.format().as_str().to_string(),
        ))
    }

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

struct ArrowIpcWriter<W: Write + Send> {
    schema: SchemaRef,
    writer: StreamWriter<W>,
}

impl<W: Write + Send> CodecWriter for ArrowIpcWriter<W> {
    fn write_batch(&mut self, batch: &RecordBatch) -> Result<()> {
        validate_batch(&self.schema, batch)?;
        self.writer.write(batch)?;
        self.writer.flush()?;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        self.writer.finish()?;
        Ok(())
    }
}

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
        let mut writer = self.start_writer(schema, output)?;
        for batch in batches {
            writer.write_batch(&batch?)?;
        }
        writer.finish()
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

    fn start_writer<'a>(
        &self,
        schema: SchemaRef,
        output: &'a mut (dyn Write + Send),
    ) -> Result<Box<dyn CodecWriter + 'a>> {
        let writer = StreamWriter::try_new(output, schema.as_ref())?;
        Ok(Box::new(ArrowIpcWriter { schema, writer }))
    }

    fn start_owned_writer(
        &self,
        schema: SchemaRef,
        output: Box<dyn Write + Send>,
    ) -> Result<Box<dyn CodecWriter>> {
        let writer = StreamWriter::try_new(output, schema.as_ref())?;
        Ok(Box::new(ArrowIpcWriter { schema, writer }))
    }
}

pub struct ParquetCodec;

struct ParquetWriter<W: Write + Send> {
    schema: SchemaRef,
    writer: ArrowWriter<W>,
}

impl<W: Write + Send> CodecWriter for ParquetWriter<W> {
    fn write_batch(&mut self, batch: &RecordBatch) -> Result<()> {
        validate_batch(&self.schema, batch)?;
        self.writer.write(batch)?;
        self.writer.flush()?;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        self.writer.finish()?;
        Ok(())
    }
}

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
        let mut writer = self.start_writer(schema, output)?;
        for batch in batches {
            writer.write_batch(&batch?)?;
        }
        writer.finish()
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

    fn start_writer<'a>(
        &self,
        schema: SchemaRef,
        output: &'a mut (dyn Write + Send),
    ) -> Result<Box<dyn CodecWriter + 'a>> {
        let writer = ArrowWriter::try_new(output, schema.clone(), None)?;
        Ok(Box::new(ParquetWriter { schema, writer }))
    }

    fn start_owned_writer(
        &self,
        schema: SchemaRef,
        output: Box<dyn Write + Send>,
    ) -> Result<Box<dyn CodecWriter>> {
        let writer = ArrowWriter::try_new(output, schema.clone(), None)?;
        Ok(Box::new(ParquetWriter { schema, writer }))
    }
}

pub struct CsvCodec;

struct CsvCodecWriter<W: Write + Send> {
    schema: SchemaRef,
    writer: CsvWriter<W>,
}

impl<W: Write + Send> CodecWriter for CsvCodecWriter<W> {
    fn write_batch(&mut self, batch: &RecordBatch) -> Result<()> {
        validate_batch(&self.schema, batch)?;
        self.writer.write(batch)?;
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

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
        let mut writer = self.start_writer(schema, output)?;
        for batch in batches {
            writer.write_batch(&batch?)?;
        }
        writer.finish()
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

    fn start_writer<'a>(
        &self,
        schema: SchemaRef,
        output: &'a mut (dyn Write + Send),
    ) -> Result<Box<dyn CodecWriter + 'a>> {
        let writer = CsvWriterBuilder::new().build(output);
        Ok(Box::new(CsvCodecWriter { schema, writer }))
    }

    fn start_owned_writer(
        &self,
        schema: SchemaRef,
        output: Box<dyn Write + Send>,
    ) -> Result<Box<dyn CodecWriter>> {
        let writer = CsvWriterBuilder::new().build(output);
        Ok(Box::new(CsvCodecWriter { schema, writer }))
    }
}

pub struct JsonLinesCodec;

struct JsonLinesCodecWriter<W: Write + Send> {
    schema: SchemaRef,
    writer: LineDelimitedWriter<W>,
}

impl<W: Write + Send> CodecWriter for JsonLinesCodecWriter<W> {
    fn write_batch(&mut self, batch: &RecordBatch) -> Result<()> {
        validate_batch(&self.schema, batch)?;
        self.writer.write(batch)?;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        self.writer.finish()?;
        Ok(())
    }
}

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
        let mut writer = self.start_writer(schema, output)?;
        for batch in batches {
            writer.write_batch(&batch?)?;
        }
        writer.finish()
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

    fn start_writer<'a>(
        &self,
        schema: SchemaRef,
        output: &'a mut (dyn Write + Send),
    ) -> Result<Box<dyn CodecWriter + 'a>> {
        let writer = LineDelimitedWriter::new(output);
        Ok(Box::new(JsonLinesCodecWriter { schema, writer }))
    }

    fn start_owned_writer(
        &self,
        schema: SchemaRef,
        output: Box<dyn Write + Send>,
    ) -> Result<Box<dyn CodecWriter>> {
        let writer = LineDelimitedWriter::new(output);
        Ok(Box::new(JsonLinesCodecWriter { schema, writer }))
    }
}

fn required_schema(format: DataFormat, schema: Option<SchemaRef>) -> Result<SchemaRef> {
    schema.ok_or_else(|| InterchangeError::DecodeSchemaRequired(format.as_str().to_string()))
}

fn validate_batch(schema: &SchemaRef, batch: &RecordBatch) -> Result<()> {
    if batch.schema().as_ref() != schema.as_ref() {
        return Err(InterchangeError::InputSchema);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Mutex;

    use arrow::array::{Int64Array, RecordBatch, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};

    use super::*;

    #[derive(Clone, Default)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl SharedSink {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }

        fn len(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    impl Write for SharedSink {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

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
    fn resumable_writers_encode_batches_incrementally() {
        let (schema, batches) = fixture();

        for format in [
            DataFormat::Arrow,
            DataFormat::Parquet,
            DataFormat::Csv,
            DataFormat::JsonLines,
        ] {
            let codec = DEFAULT_REGISTRY.get(format).unwrap();
            let mut sink = SharedSink::default();
            let observed = sink.clone();
            let mut writer = codec.start_writer(schema.clone(), &mut sink).unwrap();
            let header_len = observed.len();
            writer.write_batch(&batches[0]).unwrap();
            let first_len = observed.len();
            writer.write_batch(&batches[1]).unwrap();
            let second_len = observed.len();
            writer.finish().unwrap();

            match format {
                DataFormat::Arrow | DataFormat::Csv | DataFormat::JsonLines => {
                    assert!(first_len > header_len);
                    assert!(second_len > first_len);
                }
                DataFormat::Parquet => {
                    assert_eq!(first_len, header_len);
                    assert_eq!(second_len, first_len);
                }
            }
            match format {
                DataFormat::Arrow | DataFormat::Parquet => {
                    assert!(observed.len() > second_len);
                }
                DataFormat::Csv | DataFormat::JsonLines => {
                    assert_eq!(observed.len(), second_len);
                }
            }
            let encoded = observed.bytes();
            let decode_schema = match format {
                DataFormat::Csv | DataFormat::JsonLines => Some(schema.clone()),
                DataFormat::Arrow | DataFormat::Parquet => None,
            };
            let decoded = codec.decode(&encoded, decode_schema).unwrap();
            let combined = arrow::compute::concat_batches(&schema, &decoded.batches).unwrap();
            let expected = arrow::compute::concat_batches(&schema, &batches).unwrap();
            assert_eq!(combined, expected);
        }
    }

    #[test]
    fn resumable_writers_reject_mismatched_batches() {
        let (schema, _) = fixture();
        let mismatched = RecordBatch::new_empty(Arc::new(Schema::empty()));

        for format in [
            DataFormat::Arrow,
            DataFormat::Parquet,
            DataFormat::Csv,
            DataFormat::JsonLines,
        ] {
            let codec = DEFAULT_REGISTRY.get(format).unwrap();
            let mut encoded = Vec::new();
            let mut writer = codec.start_writer(schema.clone(), &mut encoded).unwrap();
            let error = writer.write_batch(&mismatched).unwrap_err().to_string();

            assert!(error.contains("input schema"));
        }
    }

    #[test]
    fn owned_writers_can_outlive_the_sink_binding() {
        let (schema, batches) = fixture();

        for format in [
            DataFormat::Arrow,
            DataFormat::Parquet,
            DataFormat::Csv,
            DataFormat::JsonLines,
        ] {
            let codec = DEFAULT_REGISTRY.get(format).unwrap();
            let sink = SharedSink::default();
            let observed = sink.clone();
            let mut writer = codec
                .start_owned_writer(schema.clone(), Box::new(sink))
                .unwrap();
            for batch in &batches {
                writer.write_batch(batch).unwrap();
            }
            writer.finish().unwrap();

            let decode_schema = match format {
                DataFormat::Csv | DataFormat::JsonLines => Some(schema.clone()),
                DataFormat::Arrow | DataFormat::Parquet => None,
            };
            let decoded = codec.decode(&observed.bytes(), decode_schema).unwrap();
            let combined = arrow::compute::concat_batches(&schema, &decoded.batches).unwrap();
            let expected = arrow::compute::concat_batches(&schema, &batches).unwrap();
            assert_eq!(combined, expected);
        }
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
