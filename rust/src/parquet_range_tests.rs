use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use arrow::array::{Int32Array, RecordBatch};
use bytes::Bytes;
use fsspec_rs::{OperationError, S3Config, S3Fs};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;

use crate::{
    CancellationToken, DataSource, FileDataSource, InterchangeError, ScanSelection, StreamOptions,
};

#[derive(Debug)]
struct Request {
    method: String,
    range: Option<(usize, usize)>,
    condition: Option<String>,
}

struct Server {
    endpoint: String,
    requests: Arc<Mutex<Vec<Request>>>,
    changed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn new(data: Vec<u8>, change_after_footer: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let changed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (recorded, replaced, stopped) = (requests.clone(), changed.clone(), stop.clone());
        let thread = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let method = line.split_whitespace().next().unwrap().to_owned();
                let mut range = None;
                let mut condition = None;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap();
                    if name.eq_ignore_ascii_case("range") {
                        let (start, end) = value
                            .trim()
                            .strip_prefix("bytes=")
                            .unwrap()
                            .split_once('-')
                            .unwrap();
                        range = Some((
                            start.parse::<usize>().unwrap(),
                            end.parse::<usize>().unwrap() + 1,
                        ));
                    } else if name.eq_ignore_ascii_case("if-match") {
                        condition = Some(value.trim().to_owned());
                    }
                }
                let etag = if replaced.load(Ordering::Acquire) {
                    "\"v2\""
                } else {
                    "\"v1\""
                };
                let conflict = condition.as_ref().is_some_and(|expected| expected != etag);
                recorded.lock().unwrap().push(Request {
                    method: method.clone(),
                    range,
                    condition,
                });
                if conflict {
                    write!(socket, "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    continue;
                }
                let (start, end) = range.unwrap_or((0, data.len()));
                let status = if range.is_some() {
                    "206 Partial Content"
                } else {
                    "200 OK"
                };
                write!(socket, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nETag: {etag}\r\nLast-Modified: Tue, 17 Nov 2026 12:00:00 GMT\r\nConnection: close\r\n", end - start).unwrap();
                if range.is_some() {
                    write!(
                        socket,
                        "Content-Range: bytes {start}-{}/{len}\r\n",
                        end - 1,
                        len = data.len()
                    )
                    .unwrap();
                }
                write!(socket, "\r\n").unwrap();
                if method == "GET" {
                    if change_after_footer {
                        replaced.store(true, Ordering::Release);
                    }
                    socket.write_all(&data[start..end]).unwrap();
                }
            }
        });
        Self {
            endpoint,
            requests,
            changed,
            stop,
            thread: Some(thread),
        }
    }

    fn source(&self) -> FileDataSource {
        let mut config = S3Config::new("test");
        config.anon = true;
        config.region = Some("us-east-1".into());
        config.endpoint_url = Some(self.endpoint.clone());
        FileDataSource::new(Arc::new(S3Fs::new(config).unwrap()))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn parquet_file() -> Vec<u8> {
    let batch = RecordBatch::try_from_iter([
        (
            "a",
            Arc::new(Int32Array::from_iter_values(0..12)) as arrow::array::ArrayRef,
        ),
        ("b", Arc::new(Int32Array::from_iter_values(20..32))),
        ("unused", Arc::new(Int32Array::from_iter_values(40..52))),
    ])
    .unwrap();
    let mut data = Vec::new();
    let properties = WriterProperties::builder()
        .set_max_row_group_row_count(Some(4))
        .build();
    let mut writer = ArrowWriter::try_new(&mut data, batch.schema(), Some(properties)).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
    data
}

#[test]
fn s3_parquet_coalesces_selected_chunks_and_pins_footer_and_data() {
    let data = parquet_file();
    let builder = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(data.clone())).unwrap();
    let group = builder.metadata().row_group(1);
    let start = group.column(0).byte_range().0 as usize;
    let (last_start, last_len) = group.column(1).byte_range();
    let end = (last_start + last_len) as usize;
    let server = Server::new(data, false);
    let mut stream = server
        .source()
        .scan_selected(
            "rows.parquet",
            &ScanSelection {
                columns: Some(vec!["b".into(), "a".into(), "b".into()]),
                row_offset: 5,
            },
            StreamOptions {
                row_limit: Some(2),
                ..Default::default()
            },
            CancellationToken::new(),
        )
        .unwrap();
    let initial_requests = server.requests.lock().unwrap().len();
    let batch = stream.next().unwrap().unwrap();
    assert_eq!(batch.num_rows(), 2);
    assert_eq!(batch.num_columns(), 3);
    assert_eq!(
        batch
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .values()
            .as_ref(),
        &[25, 26]
    );
    assert_eq!(
        batch
            .column(1)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .values()
            .as_ref(),
        &[5, 6]
    );
    assert_eq!(batch.column(0), batch.column(2));
    assert!(stream.next().is_none());
    let requests = server.requests.lock().unwrap();
    let data_gets: Vec<_> = requests[initial_requests..]
        .iter()
        .filter(|r| r.method == "GET")
        .collect();
    assert_eq!(data_gets.len(), 1);
    assert_eq!(data_gets[0].range, Some((start, end)));
    let first_get = requests.iter().position(|r| r.method == "GET").unwrap();
    assert!(requests[first_get..]
        .iter()
        .all(|r| r.condition.as_deref() == Some("\"v1\"")));
}

#[test]
fn s3_parquet_rejects_change_between_footer_requests() {
    let server = Server::new(parquet_file(), true);
    let result = server.source().scan(
        "rows.parquet",
        StreamOptions::default(),
        CancellationToken::new(),
    );
    assert!(matches!(
        result,
        Err(InterchangeError::Operation(OperationError::Conflict(_)))
    ));
}

#[test]
fn s3_parquet_rejects_change_before_data_and_between_row_groups() {
    for read_first_group in [false, true] {
        let server = Server::new(parquet_file(), false);
        let mut stream = server
            .source()
            .scan(
                "rows.parquet",
                StreamOptions {
                    batch_size: 4,
                    ..Default::default()
                },
                CancellationToken::new(),
            )
            .unwrap();
        if read_first_group {
            assert_eq!(stream.next().unwrap().unwrap().num_rows(), 4);
        }
        server.changed.store(true, Ordering::Release);
        assert!(matches!(
            stream.next(),
            Some(Err(InterchangeError::Operation(OperationError::Conflict(
                _
            ))))
        ));
        assert!(stream.next().is_none());
    }
}

#[test]
fn memory_parquet_scan_runs_inside_existing_executor() {
    use fsspec_rs::{FileSystem, MemoryFs};
    let fs = MemoryFs::new();
    fs.pipe_file("/nested.parquet", &parquet_file()).unwrap();
    let source = FileDataSource::new(Arc::new(fs));
    futures::executor::block_on(async {
        let stream = source
            .scan(
                "/nested.parquet",
                StreamOptions::default(),
                CancellationToken::new(),
            )
            .unwrap();
        let batches = stream.collect_batches().unwrap();
        assert_eq!(
            batches.iter().map(|batch| batch.num_rows()).sum::<usize>(),
            12
        );
    });
}
