//! The S3 backend against a real MinIO server: a repository kept in a bucket, restored from a
//! fresh connection, with duplicate uploads, an interrupted backup, a network outage in the
//! middle of a backup, and connection failures.
//!
//! These tests need a server, which this repository does not start by itself. They run only when
//! `VAULTSNAP_S3_TEST_ENDPOINT` names one (for example `http://127.0.0.1:21100`); otherwise each
//! test prints `SKIPPED` and passes, and a skipped test is **unverified**, not successful.
//! `docs/S3.md` has the exact commands, including the Docker Compose file for MinIO and the
//! environment variables:
//!
//! * `VAULTSNAP_S3_TEST_ENDPOINT`: the server URL (required).
//! * `VAULTSNAP_S3_TEST_BUCKET`: an existing bucket; `vaultsnap-test` if unset.
//! * Credentials come from the AWS credential chain, as they do for the program itself
//!   (`AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`).
//!
//! Every test keeps its repository under a prefix of its own, so the tests can run side by side,
//! and removes it again when it ends. The outage test puts a small TCP forwarder between the
//! backend and MinIO on a loopback port from 21150 to 21199 and cuts it on purpose; the
//! connection-refused case uses a free port from 21110 to 21149.
//!
//! The behavior of the backend itself (retries, limits, conditional writes) is tested without a
//! network next to the code, in `src/storage/s3/`.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vaultsnap_core::storage::DirEntryKind;
use vaultsnap_core::{
    Backend, BackupEvent, BackupOptions, CheckOptions, ChunkerConfig, Error, KdfParams, Passphrase,
    Repository, RepositoryKeys, RestoreOptions, RetryPolicy, S3Backend, S3Config, SnapshotId,
    SnapshotRef,
};

type Tree = BTreeMap<String, Vec<u8>>;

const PASSPHRASE: &[u8] = b"correct horse battery staple";

/// Text that must never be visible in the bucket.
const PLAINTEXT_MARKER: &[u8] = b"VAULTSNAP-PLAINTEXT-MARKER-0451";

// ---------------------------------------------------------------------------------------------
// Which server, and whether there is one
// ---------------------------------------------------------------------------------------------

struct Minio {
    endpoint: String,
    bucket: String,
}

fn minio() -> Option<Minio> {
    let endpoint = std::env::var("VAULTSNAP_S3_TEST_ENDPOINT")
        .ok()
        .filter(|value| !value.is_empty())?;
    let bucket =
        std::env::var("VAULTSNAP_S3_TEST_BUCKET").unwrap_or_else(|_| "vaultsnap-test".to_owned());
    Some(Minio { endpoint, bucket })
}

/// Binds `$name` to the configured server, or ends the test as skipped.
macro_rules! require_minio {
    ($name:ident) => {
        let Some($name) = minio() else {
            eprintln!(
                "SKIPPED (unverified): set VAULTSNAP_S3_TEST_ENDPOINT to run against MinIO, \
                 see docs/S3.md"
            );
            return;
        };
    };
}

fn passphrase() -> Passphrase {
    Passphrase::new(PASSPHRASE.to_vec()).expect("valid passphrase")
}

fn kdf() -> KdfParams {
    KdfParams {
        m_cost_kib: 64,
        t_cost: 1,
        p_cost: 1,
    }
}

/// A configuration for `prefix` in the test bucket. Retries are quick so that failures show up
/// in seconds, not minutes.
fn config(minio: &Minio, prefix: &str) -> S3Config {
    let mut config = S3Config::new(minio.bucket.clone());
    config.prefix = prefix.to_owned();
    config.endpoint = Some(minio.endpoint.clone());
    config.region = Some("us-east-1".to_owned());
    config.path_style = Some(true);
    config.max_concurrent_requests = 4;
    config.connect_timeout = Duration::from_secs(5);
    config.request_timeout = Duration::from_secs(30);
    config.retry = RetryPolicy {
        max_attempts: 4,
        initial_delay: Duration::from_millis(20),
        max_delay: Duration::from_millis(200),
        jitter: true,
    };
    config
}

/// A prefix no other test, and no earlier run, uses.
fn unique_prefix(test: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("vaultsnap-test-{test}-{}-{nanos}", std::process::id())
}

/// Connects on a blocking thread, as the program does: the S3 client has a runtime of its own.
async fn try_connect(config: S3Config) -> Result<Arc<dyn Backend>, Error> {
    tokio::task::spawn_blocking(move || S3Backend::connect(config))
        .await
        .expect("the connecting thread does not panic")
}

async fn connect(config: S3Config) -> Arc<dyn Backend> {
    try_connect(config).await.expect("connect to MinIO")
}

/// A repository location of its own in the bucket, emptied again when the test ends.
struct Site {
    config: S3Config,
    backend: Arc<dyn Backend>,
}

impl Site {
    async fn new(minio: &Minio, test: &str) -> Self {
        let config = config(minio, &unique_prefix(test));
        let backend = connect(config.clone()).await;
        Self { config, backend }
    }

    fn repository(&self) -> Repository {
        Repository::with_backend(self.backend.clone())
    }

    /// A second, independent client of the same location, reaching the server at `endpoint`.
    async fn repository_via(&self, endpoint: &str) -> Repository {
        let mut config = self.config.clone();
        config.endpoint = Some(endpoint.to_owned());
        Repository::with_backend(connect(config).await)
    }

    /// The names of all objects below `dir`, at any depth.
    fn objects(&self, dir: &str) -> Vec<String> {
        walk(self.backend.as_ref(), dir).expect("list the bucket")
    }
}

impl Drop for Site {
    fn drop(&mut self) {
        if let Ok(names) = walk(self.backend.as_ref(), "") {
            for name in names {
                let _ = self.backend.delete(&name);
            }
        }
    }
}

fn walk(backend: &dyn Backend, dir: &str) -> Result<Vec<String>, Error> {
    let mut names = Vec::new();
    for entry in backend.list_dir(dir)? {
        let name = if dir.is_empty() {
            entry.name
        } else {
            format!("{dir}/{}", entry.name)
        };
        if entry.kind == DirEntryKind::Directory {
            names.extend(walk(backend, &name)?);
        } else {
            names.push(name);
        }
    }
    Ok(names)
}

// ---------------------------------------------------------------------------------------------
// Test data and helpers
// ---------------------------------------------------------------------------------------------

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn write(root: &Path, rel: &str, contents: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parents");
    }
    fs::write(path, contents).expect("write file");
}

/// Every regular file below `root` with its bytes, by path relative to `root`.
fn read_tree(root: &Path) -> Tree {
    let mut tree = Tree::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path.strip_prefix(root).expect("inside");
                tree.insert(
                    relative.to_string_lossy().into_owned(),
                    fs::read(&path).expect("read"),
                );
            }
        }
    }
    tree
}

/// A source with `files` random files and one that holds [`PLAINTEXT_MARKER`].
fn make_source(work: &Path, files: u64) -> PathBuf {
    let src = work.join("src");
    write(&src, "notes/marker.txt", &PLAINTEXT_MARKER.repeat(50));
    for i in 0..files {
        write(
            &src,
            &format!("data/file{i:02}.bin"),
            &pseudo_random(30_000, 100 + i),
        );
    }
    src
}

fn options(src: &Path) -> BackupOptions {
    BackupOptions {
        sources: vec![src.to_path_buf()],
        chunker: ChunkerConfig::new(1024, 4096, 16_384).expect("valid chunker limits"),
        host: "minio-test".to_owned(),
        jobs: 2,
        racy_margin: Duration::ZERO,
        ..BackupOptions::default()
    }
}

/// Restores a snapshot of `src` into `out` and returns the restored files.
async fn restored_files(
    repo: &Repository,
    keys: &RepositoryKeys,
    id: &SnapshotId,
    src: &Path,
    out: &Path,
) -> Tree {
    let options = RestoreOptions::new(SnapshotRef::from(id), out);
    let report = repo.restore(keys, options).await.expect("restore succeeds");
    assert!(report.is_complete(), "{report:?}");
    let canonical = fs::canonicalize(src).expect("the source exists");
    read_tree(&report.target.join(canonical.strip_prefix("/").expect("absolute")))
}

/// A full check that finds no missing chunk and no damaged snapshot. (Leftovers such as the lock
/// file of a backup that was cut off are warnings, which this does not look at.)
async fn assert_no_damage(repo: &Repository, keys: &RepositoryKeys) {
    let options = CheckOptions {
        read_data: true,
        ..CheckOptions::default()
    };
    let report = repo.check(keys, options).await.expect("the check runs");
    assert_eq!(report.stats.chunks_missing, 0, "{:?}", report.findings);
    assert_eq!(report.stats.snapshots_damaged, 0, "{:?}", report.findings);
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// The secrets the AWS credential chain may have been given through the environment.
fn secrets() -> Vec<String> {
    ["AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .filter(|value| value.len() >= 4)
        .collect()
}

/// An error, shown the ways a program shows it, must not contain a credential.
fn assert_no_credentials(error: &Error, config: &S3Config) {
    for text in [
        error.to_string(),
        format!("{error:?}"),
        format!("{config:?}"),
    ] {
        for secret in secrets() {
            assert!(!text.contains(&secret), "a credential appears in: {text}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// A TCP forwarder that can be cut, to take the network away from a running backup
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct ProxyControl {
    up: AtomicBool,
    live: Mutex<Vec<TcpStream>>,
}

impl ProxyControl {
    /// Refuses new connections and closes the open ones.
    fn cut(&self) {
        self.up.store(false, Ordering::SeqCst);
        self.close_all();
    }

    fn restore(&self) {
        self.up.store(true, Ordering::SeqCst);
    }

    fn close_all(&self) {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        for stream in live.drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

struct Proxy {
    port: u16,
    control: Arc<ProxyControl>,
    stop: Arc<AtomicBool>,
    accepter: Option<JoinHandle<()>>,
}

impl Proxy {
    /// Forwards a loopback port to `upstream` (`host:port`).
    fn start(upstream: &str) -> Self {
        let listener = (21_150..=21_199)
            .find_map(|port| TcpListener::bind(("127.0.0.1", port)).ok())
            .expect("a free port from 21150 to 21199");
        let port = listener.local_addr().expect("the listener has an address").port();
        listener.set_nonblocking(true).expect("non-blocking listener");
        let control = Arc::new(ProxyControl::default());
        control.restore();
        let stop = Arc::new(AtomicBool::new(false));
        let accepter = {
            let (control, stop, upstream) = (control.clone(), stop.clone(), upstream.to_owned());
            thread::spawn(move || accept_loop(&listener, &control, &stop, &upstream))
        };
        Self {
            port,
            control,
            stop,
            accepter: Some(accepter),
        }
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.control.cut();
        if let Some(accepter) = self.accepter.take() {
            let _ = accepter.join();
        }
    }
}

fn accept_loop(listener: &TcpListener, control: &ProxyControl, stop: &AtomicBool, upstream: &str) {
    while !stop.load(Ordering::SeqCst) {
        let Ok((client, _)) = listener.accept() else {
            thread::sleep(Duration::from_millis(5));
            continue;
        };
        if !control.up.load(Ordering::SeqCst) {
            continue;
        }
        // A socket accepted from a non-blocking listener may itself be non-blocking.
        let _ = client.set_nonblocking(false);
        let Ok(server) = TcpStream::connect(upstream) else {
            continue;
        };
        let clones = (
            client.try_clone(),
            server.try_clone(),
            client.try_clone(),
            server.try_clone(),
        );
        let (Ok(client_reader), Ok(server_reader), Ok(client_handle), Ok(server_handle)) = clones
        else {
            continue;
        };
        control
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend([client_handle, server_handle]);
        // A cut that came between the check above and the registration must not be missed.
        if !control.up.load(Ordering::SeqCst) {
            control.close_all();
        }
        thread::spawn(move || pump(client_reader, server));
        thread::spawn(move || pump(server_reader, client));
    }
}

fn pump(mut from: TcpStream, mut to: TcpStream) {
    let _ = io::copy(&mut from, &mut to);
    let _ = to.shutdown(Shutdown::Both);
    let _ = from.shutdown(Shutdown::Both);
}

/// A loopback port from 21110 to 21149 that nothing listens on.
fn closed_port() -> u16 {
    (21_110..=21_149)
        .find(|port| TcpListener::bind(("127.0.0.1", *port)).is_ok())
        .expect("a free port from 21110 to 21149")
}

// ---------------------------------------------------------------------------------------------
// Restoring from remote storage
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_repository_in_a_bucket_is_restored_from_a_fresh_connection() {
    require_minio!(minio);
    let site = Site::new(&minio, "roundtrip").await;
    let work = tempfile::tempdir().expect("temporary directory");
    let src = make_source(work.path(), 8);
    let expected = read_tree(&src);

    let repo = site.repository();
    repo.init_with_kdf(&passphrase(), kdf()).await.expect("init");
    let keys = repo.unlock(&passphrase()).await.expect("unlock");
    let report = repo.backup(&keys, options(&src)).await.expect("backup");
    assert!(report.transfer.chunks_new > 0);
    assert!(!report.is_partial());

    // Restoring needs nothing but the bucket location and the passphrase.
    let other = site.repository_via(&minio.endpoint).await;
    let wrong = Passphrase::new(b"not the passphrase".to_vec()).expect("valid passphrase");
    assert!(other.unlock(&wrong).await.is_err(), "a wrong passphrase must not unlock");
    let keys = other.unlock(&passphrase()).await.expect("unlock from the bucket alone");
    let listing = other.snapshots(&keys).await.expect("list");
    assert!(listing.damaged.is_empty());
    assert_eq!(listing.snapshots.len(), 1);
    let restored =
        restored_files(&other, &keys, &report.snapshot, &src, &work.path().join("out")).await;
    assert_eq!(restored, expected, "the restored files equal the originals byte for byte");

    let check = CheckOptions {
        read_data: true,
        ..CheckOptions::default()
    };
    let checked = other.check(&keys, check).await.expect("the check runs");
    assert!(checked.is_clean(), "{:?}", checked.findings);
}

#[tokio::test]
async fn nothing_readable_is_uploaded() {
    require_minio!(minio);
    let site = Site::new(&minio, "encrypted").await;
    let work = tempfile::tempdir().expect("temporary directory");
    let src = make_source(work.path(), 3);
    let first_file = read_tree(&src)
        .into_iter()
        .find(|(name, _)| name.ends_with("file00.bin"))
        .expect("the source has the file")
        .1;

    let repo = site.repository();
    repo.init_with_kdf(&passphrase(), kdf()).await.expect("init");
    let keys = repo.unlock(&passphrase()).await.expect("unlock");
    repo.backup(&keys, options(&src)).await.expect("backup");

    let names = site.objects("");
    assert!(names.iter().any(|name| name.starts_with("data/")));
    assert!(names.iter().any(|name| name.starts_with("snapshots/")));
    assert!(names.iter().any(|name| name.starts_with("keys/")));
    for name in &names {
        let object = site.backend.get(name, 64 << 20).expect("read the object back");
        for secret in [
            PLAINTEXT_MARKER,
            b"marker.txt".as_slice(),
            b"file00.bin".as_slice(),
            b"minio-test".as_slice(),
            PASSPHRASE,
            &first_file[..64],
        ] {
            assert!(
                !contains(&object.data, secret),
                "{name} contains {:?}",
                String::from_utf8_lossy(secret)
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Duplicate uploads
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn data_that_is_already_in_the_bucket_is_not_uploaded_again() {
    require_minio!(minio);
    let site = Site::new(&minio, "duplicates").await;
    let work = tempfile::tempdir().expect("temporary directory");
    let src = make_source(work.path(), 6);

    let repo = site.repository();
    repo.init_with_kdf(&passphrase(), kdf()).await.expect("init");
    let keys = repo.unlock(&passphrase()).await.expect("unlock");
    let first = repo.backup(&keys, options(&src)).await.expect("first backup");
    assert!(first.transfer.chunks_new > 0);
    let objects = site.objects("data");

    let second = repo.backup(&keys, options(&src)).await.expect("second backup");
    assert_eq!(second.transfer.chunks_new, 0, "{:?}", second.transfer);
    assert_eq!(second.transfer.bytes_stored, 0, "{:?}", second.transfer);
    assert_eq!(site.objects("data"), objects, "no chunk object was added or replaced");
    assert_ne!(first.snapshot, second.snapshot);

    // Many writers storing the same new chunk: exactly one of them creates the object.
    let chunks = repo.chunk_store(&keys).expect("chunk store");
    let data = pseudo_random(10_000, 777);
    let created = thread::scope(|scope| {
        let writers: Vec<_> = (0..8)
            .map(|_| {
                let chunks = chunks.clone();
                let data = &data;
                scope.spawn(move || chunks.put(data).expect("put"))
            })
            .collect();
        writers
            .into_iter()
            .map(|writer| writer.join().expect("the writer does not panic"))
            .filter(|result| result.is_new())
            .count()
    });
    assert_eq!(created, 1, "exactly one writer creates the object");
    assert_eq!(site.objects("data").len(), objects.len() + 1);
}

// ---------------------------------------------------------------------------------------------
// Interrupted operations
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_cancelled_backup_publishes_nothing_and_the_next_run_reuses_the_uploaded_chunks() {
    require_minio!(minio);
    let site = Site::new(&minio, "cancelled").await;
    let work = tempfile::tempdir().expect("temporary directory");
    let src = make_source(work.path(), 12);
    let expected = read_tree(&src);

    let repo = site.repository();
    repo.init_with_kdf(&passphrase(), kdf()).await.expect("init");
    let keys = repo.unlock(&passphrase()).await.expect("unlock");

    let mut interrupted = options(&src);
    interrupted.jobs = 1;
    let cancel = interrupted.cancel.clone();
    let finished = Arc::new(AtomicUsize::new(0));
    interrupted.observer = Some(Arc::new(move |event: &BackupEvent<'_>| {
        if matches!(event, BackupEvent::FileFinished { .. })
            && finished.fetch_add(1, Ordering::SeqCst) + 1 == 3
        {
            cancel.cancel();
        }
    }));
    let err = repo.backup(&keys, interrupted).await.expect_err("cancelled");
    assert!(matches!(err, Error::Cancelled), "{err}");

    let listing = repo.snapshots(&keys).await.expect("list");
    assert!(listing.snapshots.is_empty(), "no snapshot is visible");
    assert!(site.objects("snapshots").is_empty(), "no snapshot object exists");
    assert!(site.objects("locks").is_empty(), "the lock was released");
    let uploaded = site.objects("data").len();
    assert!(uploaded >= 3, "the chunks of the finished files stay: {uploaded}");

    let report = repo.backup(&keys, options(&src)).await.expect("the next run succeeds");
    assert!(
        report.transfer.chunks_existing as usize >= uploaded,
        "{:?} against {uploaded} uploaded chunks",
        report.transfer
    );
    let restored =
        restored_files(&repo, &keys, &report.snapshot, &src, &work.path().join("out")).await;
    assert_eq!(restored, expected);
}

#[tokio::test]
async fn a_network_outage_during_a_backup_publishes_nothing_and_the_next_run_resumes() {
    require_minio!(minio);
    let Some(upstream) = minio.endpoint.strip_prefix("http://") else {
        eprintln!("SKIPPED (unverified): the outage test forwards plain HTTP to MinIO");
        return;
    };
    let site = Site::new(&minio, "outage").await;
    let work = tempfile::tempdir().expect("temporary directory");
    let src = make_source(work.path(), 12);
    let expected = read_tree(&src);

    let direct = site.repository();
    direct.init_with_kdf(&passphrase(), kdf()).await.expect("init");
    let direct_keys = direct.unlock(&passphrase()).await.expect("unlock");

    let proxy = Proxy::start(upstream);
    let through = site.repository_via(&proxy.endpoint()).await;
    let keys = through.unlock(&passphrase()).await.expect("unlock through the forwarder");

    let mut doomed = options(&src);
    doomed.jobs = 1;
    let control = proxy.control.clone();
    let finished = Arc::new(AtomicUsize::new(0));
    doomed.observer = Some(Arc::new(move |event: &BackupEvent<'_>| {
        if matches!(event, BackupEvent::FileFinished { .. })
            && finished.fetch_add(1, Ordering::SeqCst) + 1 == 3
        {
            control.cut();
        }
    }));
    let started = Instant::now();
    let err = through.backup(&keys, doomed).await.expect_err("the backup cannot finish");
    assert!(!matches!(err, Error::Cancelled), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(120),
        "the failure took {:?}",
        started.elapsed()
    );
    assert_no_credentials(&err, &site.config);

    // The server was never down, only the way to it. Nothing half-done is visible.
    proxy.control.restore();
    let listing = direct.snapshots(&direct_keys).await.expect("list");
    assert!(listing.snapshots.is_empty(), "no snapshot is visible");
    assert!(site.objects("snapshots").is_empty());
    let uploaded = site.objects("data").len();
    assert!(uploaded >= 3, "the chunks uploaded before the outage stay: {uploaded}");

    // Once the network is back, a new run reuses what was uploaded and completes.
    let again = site.repository_via(&proxy.endpoint()).await;
    let again_keys = again.unlock(&passphrase()).await.expect("unlock again");
    let report = again.backup(&again_keys, options(&src)).await.expect("the next run succeeds");
    assert!(
        report.transfer.chunks_existing as usize >= uploaded,
        "{:?} against {uploaded} uploaded chunks",
        report.transfer
    );
    assert_no_damage(&direct, &direct_keys).await;
    let restored = restored_files(
        &direct,
        &direct_keys,
        &report.snapshot,
        &src,
        &work.path().join("out"),
    )
    .await;
    assert_eq!(restored, expected);
}

// ---------------------------------------------------------------------------------------------
// Connection failures
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn connection_failures_are_reported_promptly_and_never_show_credentials() {
    require_minio!(minio);
    let started = Instant::now();

    // Nothing listens on this port.
    let mut refused = config(&minio, "refused");
    refused.endpoint = Some(format!("http://127.0.0.1:{}", closed_port()));
    let err = try_connect(refused.clone()).await.expect_err("connection refused");
    assert_no_credentials(&err, &refused);

    // The server answers, but there is no such bucket.
    let mut missing = config(&minio, "missing");
    missing.bucket = "vaultsnap-no-such-bucket".to_owned();
    let err = try_connect(missing.clone()).await.expect_err("no such bucket");
    assert_no_credentials(&err, &missing);

    assert!(
        started.elapsed() < Duration::from_secs(60),
        "failing took {:?}",
        started.elapsed()
    );
}
