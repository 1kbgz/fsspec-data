use std::io::{BufReader, Cursor, Write};

use arrow::array::RecordBatch;
use arrow::compute::concat_batches;
use arrow::datatypes::SchemaRef;
use arrow::error::ArrowError;
use arrow_avro::reader::{Decoder, DecoderMode, ReaderBuilder};
use arrow_avro::schema::{AvroSchema, Fingerprint, FingerprintAlgorithm, SchemaStore};
use arrow_avro::writer::AvroWriter;

use crate::codec::{validate_batch, Codec, CodecCapabilities, CodecReader, CodecWriter};
use crate::{
    CancellationToken, DataFormat, DecodedStream, InterchangeError, Result, StreamOptions,
};

/// Avro object container files, which carry their own schema.
pub struct AvroCodec;

struct AvroCodecWriter<W: Write + Send> {
    schema: SchemaRef,
    writer: AvroWriter<W>,
}

impl<W: Write + Send> CodecWriter for AvroCodecWriter<W> {
    fn write_batch(&mut self, batch: &RecordBatch) -> Result<()> {
        validate_batch(&self.schema, batch)?;
        self.writer.write(batch).map_err(ArrowError::from)?;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        self.writer.finish().map_err(ArrowError::from)?;
        Ok(())
    }
}

impl Codec for AvroCodec {
    fn format(&self) -> DataFormat {
        DataFormat::Avro
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
        self.decode_reader(Box::new(Cursor::new(data)), None, options, cancellation)
    }

    fn decode_reader(
        &self,
        reader: Box<dyn CodecReader>,
        _schema: Option<SchemaRef>,
        options: StreamOptions,
        cancellation: CancellationToken,
    ) -> Result<DecodedStream> {
        let reader = ReaderBuilder::new()
            .with_batch_size(options.batch_size)
            .build(BufReader::new(reader))?;
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
        let writer = AvroWriter::new(output, schema.as_ref().clone()).map_err(ArrowError::from)?;
        Ok(Box::new(AvroCodecWriter { schema, writer }))
    }

    fn start_owned_writer(
        &self,
        schema: SchemaRef,
        output: Box<dyn Write + Send>,
    ) -> Result<Box<dyn CodecWriter>> {
        let writer = AvroWriter::new(output, schema.as_ref().clone()).map_err(ArrowError::from)?;
        Ok(Box::new(AvroCodecWriter { schema, writer }))
    }
}

/// How Avro values are framed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AvroFraming {
    /// Bare datums written with the reader schema.
    Unframed,
    /// Confluent wire format: a zero magic byte and a big-endian schema ID before each datum.
    /// Holds the writer schema JSON for every ID that may appear.
    Confluent(Vec<(u32, String)>),
}

/// The Arrow schema that values with the Avro schema `schema` (JSON) decode to.
pub fn avro_arrow_schema(schema: &str) -> Result<SchemaRef> {
    Ok(AvroValueDecoder::new(schema, AvroFraming::Unframed, 1)?.schema())
}

/// Decodes individual Avro values, such as message payloads, into record batches.
/// Values written with an older writer schema are resolved to the reader schema.
pub struct AvroValueDecoder {
    decoder: Decoder,
    framed: bool,
    /// Rows set aside when the decoder stopped to switch writer schemas.
    decoded: Vec<RecordBatch>,
}

impl AvroValueDecoder {
    /// Decodes values with the Avro reader schema `schema` (JSON). `batch_size` sets how
    /// many rows the decoder buffers internally; `flush` returns every row pushed since the
    /// last flush.
    pub fn new(schema: &str, framing: AvroFraming, batch_size: usize) -> Result<Self> {
        let reader_schema = AvroSchema::new(schema.to_string());
        let builder = ReaderBuilder::new()
            .with_batch_size(batch_size)
            .with_reader_schema(reader_schema.clone());
        let framed = matches!(framing, AvroFraming::Confluent(_));
        let builder = match framing {
            AvroFraming::Unframed => {
                let mut store = SchemaStore::new();
                let fingerprint = store.register(reader_schema)?;
                builder
                    .with_writer_schema_store(store)
                    .with_active_fingerprint(fingerprint)
                    .with_decoder_mode(DecoderMode::UnframedDatum)
            }
            AvroFraming::Confluent(writers) => {
                let mut store = SchemaStore::new_with_type(FingerprintAlgorithm::Id);
                for (id, writer) in writers {
                    store.set(Fingerprint::Id(id), AvroSchema::new(writer))?;
                }
                builder.with_writer_schema_store(store)
            }
        };
        Ok(Self {
            decoder: builder.build_decoder()?,
            framed,
            decoded: Vec::new(),
        })
    }

    pub fn schema(&self) -> SchemaRef {
        self.decoder.schema()
    }

    /// Decodes one value, which must be exactly one datum or framed message.
    pub fn push(&mut self, value: &[u8]) -> Result<()> {
        if self.decoder.batch_is_full() {
            self.set_aside()?;
        }
        let mut consumed = self.decoder.decode(value).map_err(ArrowError::from)?;
        // A framed value with a new schema ID stops after its prefix until the rows decoded
        // with the previous writer schema are flushed.
        while self.framed && consumed < value.len() {
            if !self.set_aside()? {
                break;
            }
            consumed += self
                .decoder
                .decode(&value[consumed..])
                .map_err(ArrowError::from)?;
        }
        if consumed != value.len() {
            return Err(InterchangeError::Arrow(ArrowError::ParseError(format!(
                "Avro value has {} bytes after its datum",
                value.len() - consumed
            ))));
        }
        Ok(())
    }

    /// Moves decoded rows out of the decoder, returning whether there were any.
    fn set_aside(&mut self) -> Result<bool> {
        let batch = self.decoder.flush().map_err(ArrowError::from)?;
        let found = batch.is_some();
        self.decoded.extend(batch);
        Ok(found)
    }

    /// The rows decoded since the last flush.
    pub fn flush(&mut self) -> Result<Option<RecordBatch>> {
        self.set_aside()?;
        if self.decoded.is_empty() {
            return Ok(None);
        }
        let batches = std::mem::take(&mut self.decoded);
        Ok(Some(concat_batches(&self.schema(), &batches)?))
    }
}

#[cfg(test)]
mod tests {
    use arrow::array::{AsArray, Int64Array, StringArray};
    use arrow::datatypes::{DataType, Int64Type};

    use super::*;

    const V1: &str = r#"{"type": "record", "name": "User", "fields": [
        {"name": "id", "type": "long"}]}"#;
    const V2: &str = r#"{"type": "record", "name": "User", "fields": [
        {"name": "id", "type": "long"},
        {"name": "name", "type": "string", "default": "?"}]}"#;

    /// `{"id": id, "name": name}` in Avro binary encoding, for ids and names under 64.
    fn datum(id: u8, name: Option<&str>) -> Vec<u8> {
        let mut data = vec![id * 2];
        if let Some(name) = name {
            data.push(name.len() as u8 * 2);
            data.extend_from_slice(name.as_bytes());
        }
        data
    }

    fn confluent(id: u32, datum: Vec<u8>) -> Vec<u8> {
        [&[0][..], &id.to_be_bytes(), &datum].concat()
    }

    #[test]
    fn unframed_values_decode_with_the_reader_schema() {
        let mut decoder = AvroValueDecoder::new(V2, AvroFraming::Unframed, 16).unwrap();
        decoder.push(&datum(1, Some("a"))).unwrap();
        decoder.push(&datum(2, Some("b"))).unwrap();
        let batch = decoder.flush().unwrap().unwrap();
        assert_eq!(
            batch.column(0).as_primitive::<Int64Type>(),
            &Int64Array::from(vec![1, 2])
        );
        assert_eq!(
            batch.column(1).as_string::<i32>(),
            &StringArray::from(vec!["a", "b"])
        );
        assert!(decoder.flush().unwrap().is_none());
    }

    #[test]
    fn confluent_values_resolve_older_writer_schemas() {
        let writers = vec![(1, V1.to_string()), (2, V2.to_string())];
        let mut decoder = AvroValueDecoder::new(V2, AvroFraming::Confluent(writers), 16).unwrap();
        decoder.push(&confluent(1, datum(1, None))).unwrap();
        decoder.push(&confluent(2, datum(2, Some("b")))).unwrap();
        let batch = decoder.flush().unwrap().unwrap();
        assert_eq!(
            batch.column(1).as_string::<i32>(),
            &StringArray::from(vec!["?", "b"])
        );

        assert!(decoder.push(&confluent(3, datum(3, None))).is_err());
    }

    #[test]
    fn flush_returns_every_value_pushed() {
        for framing in [
            AvroFraming::Unframed,
            AvroFraming::Confluent(vec![(2, V2.to_string())]),
        ] {
            let framed = framing != AvroFraming::Unframed;
            let mut decoder = AvroValueDecoder::new(V2, framing, 1).unwrap();
            for id in 1..=3 {
                let value = datum(id, Some("x"));
                decoder
                    .push(&if framed { confluent(2, value) } else { value })
                    .unwrap();
            }
            assert_eq!(decoder.flush().unwrap().unwrap().num_rows(), 3);
        }
    }

    #[test]
    fn values_must_be_exactly_one_datum() {
        let mut decoder = AvroValueDecoder::new(V1, AvroFraming::Unframed, 16).unwrap();
        assert!(decoder.push(&[2, 4]).is_err());
    }

    #[test]
    fn avro_schemas_map_to_arrow() {
        let schema = avro_arrow_schema(V2).unwrap();
        assert_eq!(schema.field(0).data_type(), &DataType::Int64);
        assert_eq!(schema.field(1).name(), "name");
        assert!(avro_arrow_schema("not avro").is_err());
    }
}
