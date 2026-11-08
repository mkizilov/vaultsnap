# Storage backends

Every byte of a repository is read and written through one interface, `Backend`
(`crates/vaultsnap-core/src/storage/`). The local directory is the reference implementation. The
interface is designed so that an S3-compatible object store follows without changing the
repository format or any code above it: the local directory, an in-memory object store, and an
S3-compatible bucket are built, with two wrappers (retries, fault injection). Using a bucket is
described in [`S3.md`](S3.md); this document is the reasoning and the contract.

The repository layout and what it stores are in [`FORMAT.md`](FORMAT.md); the threat model is in
[`SECURITY.md`](SECURITY.md).

## Layers

```
backup  restore  check  prune  snapshots  init  ...      operations (ops/*)
        keys  locks  ChunkStore  SnapshotStore           repository logic
                       Store                             a cheap handle: Arc<dyn Backend>
                       Backend                           the interface (this document)
   RetryingBackend    FlakyBackend                       wrappers: any backend in, a backend out
   LocalBackend       MemoryBackend       S3Backend      implementations
```

* Nothing above `Store` touches `std::fs` for repository files. `Repository::at(path)` is
  `Repository::with_backend(LocalBackend)`; `Repository::with_backend` takes any backend.
* Backends are **synchronous**. The repository calls them on Tokio's blocking pool, as it always
  has, so a network backend blocks its own thread on its own client and the runtime of the caller
  is not involved.
* The wrappers compose: `RetryingBackend::new(Arc::new(s3), RetryPolicy::standard())`.
  `S3Backend::connect` returns the S3 backend already wrapped in retries.
* The CLI selects the backend from `--repo`: a directory, or `s3://BUCKET[/FOLDER]`
  (`crates/vaultsnap-cli/src/remote.rs`).

## The model

A repository is a flat set of **objects** named by `/`-separated names such as `config`,
`keys/<id>`, `snapshots/<id>`, `data/ab/<id>`, `locks/<id>.lock`. An object is an immutable byte
string: it is written whole and replaced whole. There is no append, no rename, no hard link, no
in-place modification, no permission model, and no empty directory.

* **Names** are `/`-separated segments of `[A-Za-z0-9._-]`, each 1 to 255 bytes and not starting
  with `.`. Every backend applies `validate_name` to every name, so a name can never climb out of
  the repository, name a hidden or temporary file, or need escaping in a URL or an object key.
  A name starting with `.tmp-` is by definition a backend's own staging file, never an object.
* **Directories** are prefixes. `list_dir("data")` lists one level: objects directly below
  `data/` as files, and each next segment that has objects below it as a directory.
* A backend with `Capabilities::directories = true` (a file system) has real, possibly empty
  directories, and a write below a missing one fails. One without (an object store) has none:
  `create_dir` does nothing, `dir_exists` answers yes, listing a prefix with no objects returns
  nothing, and the repository does not look for the four standard directories.

## The interface

| Operation | Meaning | Local directory | S3 |
|-----------|---------|-----------------|----|
| `stat(name)` | size, modification time, version; not-found if absent | `lstat` | `HEAD` |
| `exists(name)` | whether it is there | `lstat` | `HEAD` (404 is "no") |
| `get(name, max_len)` | the whole object and its version; `TooLarge` beyond `max_len` | `open`, `fstat`, bounded read | `GET`, `Content-Length` checked first |
| `get_prefix(name, max)` | the first `max` bytes and the total size | bounded read | ranged `GET`, `Content-Range` for the total |
| `list_dir(dir)` | one level, sorted, with kinds and sizes | `read_dir` | `ListObjectsV2` with prefix and `/` delimiter, following continuation tokens |
| `put(name, data, Create)` | create, fail with `AlreadyExists` if present | stage a temp file, `fsync`, hard link | `PUT` with `If-None-Match: *` |
| `put(name, data, Overwrite)` | create or replace atomically | stage, `fsync`, `rename` | plain `PUT` |
| `put(name, data, Update(v))` | replace only if still version `v`, else `PreconditionFailed` | check under a process lock, then replace | `PUT` with `If-Match: <etag>` |
| `delete(name)` | remove; absent is success or not-found | `unlink` | `DELETE` (idempotent) |
| `sync_dir(dir)` | make earlier creates and deletes durable | `fsync` the directory | nothing: a completed request is durable |
| `probe()` / `create_root()` | is the place there, can it hold a repository; make it | `stat`, `mkdir -p` | `HEAD` bucket; nothing to create |
| `capabilities()` | what is guaranteed (below) | fixed | decided when connecting |
| `root()` / `local_path()` | a label for messages / the directory if there is one | the path / the path | `s3://bucket/prefix` / none |

Everything is the least common denominator of the two columns. In particular VaultSnap never needs
`rename` or hard links *as an interface*: the local backend uses them to implement an atomic
create and an atomic replace, which on an object store are one conditional `PUT` and one `PUT`.

### Writes: three conditions

`PutMode` is the whole of VaultSnap's need for atomicity.

* **`Create`** is how locks, chunk objects, snapshots, key files, and `config` are written. Exactly
  one of any number of concurrent creators of a name wins; the others get `AlreadyExists` and the
  existing object is untouched. This is what makes a snapshot commit all-or-nothing and chunk
  publication safe, and it is the one condition every repository backend must provide *globally*.
* **`Overwrite`** replaces an object so that readers see the old one or the new one, never a
  mixture. Nothing in a repository is overwritten today; it exists for metadata that will be
  (for example adding a passphrase).
* **`Update(version)`** is optimistic concurrency: replace the object only if it still has the
  version that was read. `Store::update(name, max_len, change)` wraps it in a transaction: read
  with its version, compute the new bytes, write conditionally, and start over with the new state
  if somebody got in between (at most 16 times). `change` may run more than once and must have no
  effects. The local file system has no compare-and-swap, so there it is atomic against other
  updates **in the same process only**; processes are kept apart by the repository lock.

A **version** is an opaque token compared only for equality, only for the same object from the
same backend. On S3 it is the ETag (which is not a content hash for multipart or KMS-encrypted
objects, which is why nothing may assume it is); locally it is device, inode, modification time,
and size, so replacing a file with the same bytes is a new version.

### Capabilities

A backend states what it guarantees, and must not claim more:

| Field | Meaning | Local | Memory | S3 (AWS) |
|-------|---------|-------|--------|----------|
| `read_after_write` | a completed write or delete is visible to every later read, existence check, and listing by any client | yes | yes | yes, since December 2020 |
| `create_if_absent` | how well `Create` excludes concurrent creators: none / this process / all clients | all clients | all clients | all clients (`If-None-Match`) |
| `replace_if_unchanged` | the same for `Update` | this process | all clients | all clients (`If-Match`) |
| `atomic_put` | an object appears whole or not at all, also when replacing, and a failed write leaves nothing | yes | yes | yes |
| `directories` | empty directories exist | yes | no | no |

`require_repository_support()` is checked by `init` and by every open (`load_config`, `lock`,
`check`), **before anything is read or written**. A backend is refused with `UnsupportedBackend`
and a sentence saying which guarantee is missing if:

* it is not read-after-write consistent: lock acquisition creates its own lock file and then lists
  the lock directory to find conflicting ones. If the listing could miss a lock that exists, two
  processes could each miss the other and both believe they hold the exclusive lock;
* `Create` is not exclusive for all clients: two creators could both "win" the same name, which
  breaks locks, snapshot commits, and the "never overwrite a chunk" rule;
* a write can be seen partly: nothing could be trusted without a check of every read.

`replace_if_unchanged` is not required, because nothing in a repository needs it yet;
`Store::update` fails with `UnsupportedBackend` on a backend without it.

A backend must **verify** what it claims where the other side can lie. Some S3-compatible stores
ignore the conditional headers and overwrite silently (`PUT` with `If-None-Match: *` answers
`200`). A backend for such a store must test at connection time, by writing an object twice and
requiring the second write to fail, and report `Guarantee::None` otherwise, which makes the
repository refuse it.

## Errors

Errors keep their existing types. What the repository needs to tell apart is available as:

| Question | Method | Examples |
|----------|--------|----------|
| does it simply not exist? | `Error::is_not_found()` | local `ENOENT`; S3 `404`/`NoSuchKey` |
| may the same call succeed if repeated? | `Error::is_transient()` | timeouts, resets, `5xx`, `SlowDown`, `409 ConditionalRequestConflict` |
| was a conditional write refused because of what exists? | `Error::is_conflict()` | `AlreadyExists` (create), `PreconditionFailed` (update); S3 `412` |

The local backend reports `Error::Io` (the cause is the operating system's). Every other backend
reports failures with `Error::backend(operation, name, kind, message)`, where `kind` is
`NotFound`, `Transient`, `Permission` (not retried), or `Other` (not retried). Messages contain
object names (ids and fixed words), never credentials and never names from the backed-up data.

## Retries

`RetryingBackend` wraps any backend and repeats a call that failed with a transient error,
waiting `initial_delay`, then twice that, and so on up to `max_delay`, with jitter (a random wait
between half and all of the computed one, so clients that failed together do not retry together),
for at most `max_attempts` tries. `RetryPolicy::standard()` is 5 tries starting at 100 ms with a
5 s cap. Only transient errors are retried: a missing object, a conflict, a permission error, or a
bad name is the answer.

**The hard case is a write whose answer was lost.** A timeout does not say whether the request was
carried out. If it was, the retry is refused *because the first attempt worked*:

| Retried call | What the retry sees if the first attempt landed | What the wrapper does |
|--------------|-------------------------------------------------|------------------------|
| `put(Create)` | the object it just created: `AlreadyExists` | reads it; if it holds exactly the bytes being written, the write succeeded |
| `put(Update(v))` | a version that is no longer `v`: `PreconditionFailed` | the same check |
| `delete` | the object gone: not found | success |
| `put(Overwrite)`, reads, listings | nothing special: repeating them is harmless | repeated |

Without this a successful commit would look like a lost race. The check cannot mistake another
writer for us unless that writer stored *the same bytes*; every writer here stores data unique to
it (a random lock name, a snapshot id, a freshly encrypted chunk with a random nonce), so that does
not happen. A conflict whose content differs is still reported as a conflict, and a conflict
followed by a failing read is retried as a whole.

`init` shows why this matters outside the wrapper too. If writing `config` fails in a way that
may have been a lost answer, `init` keeps the key file it wrote (an unused key file is harmless: it
fails to authenticate under another repository id and is skipped), instead of deleting what the
repository may now depend on.

Operations built from several storage calls (a backup, a prune) are **not** transactions: a
failure between calls leaves what earlier calls did. That is what the write ordering in
[`FORMAT.md`](FORMAT.md) is for: chunks first, the snapshot last on the way in, snapshots first,
chunks last on the way out, so any interruption leaves a repository in which every snapshot is
complete.

## Consistency, ordering, and time

* **Strong consistency is required, not assumed.** AWS S3 provides it. Other S3-compatible stores
  may not (listings that lag, caches in front of the store). A backend must declare
  `read_after_write: false` for those, and the repository then refuses them rather than risk a
  lock that does not exclude.
* **Listings are not snapshots in time.** A listing made while another client writes may or may
  not show that client's objects. `check` lists snapshot files before objects for this reason, and
  `prune` holds the exclusive lock.
* **Modification times** come from the store's clock. `backup` uses them only as a hint for which
  earlier snapshot to compare against; the authentic creation time is inside the manifest.
* **Locks are objects.** An exclusive lock is an object like any other, and a client that dies
  leaves it behind. Nothing expires locks; an operator removes a stale one by hand after making
  sure nothing is running. On an object store the same applies, and a bucket lifecycle rule must
  not be allowed to delete lock objects out from under a running process.
* **Versioned buckets and object lock.** `delete` on a versioned bucket only adds a delete marker;
  space is freed when a lifecycle rule expires the old versions. Retention (WORM) settings can make
  `prune` fail with a permission error, which is reported and leaves the repository consistent.

## The contract

`storage::contract` is a suite of executable checks, 22 cases, that every backend must pass
(`contract::run`, or `contract::check` to get the list of failures). It states, case by case, what
the sections above promise:

* names are validated everywhere and refused names create nothing;
* what is written reads back exactly (empty and multi-megabyte objects included) with the version
  `put` returned, `stat` reports, and `get` reads;
* `Create` never overwrites, and exactly one of 8 racing creators wins; `Update` compares
  versions, refuses stale ones and missing objects, changes nothing when refused, and exactly one
  of 8 racing updaters wins;
* a replacement is never seen torn by concurrent readers; independent creators do not interfere;
* missing objects are not-found and not transient; the size limit and prefix reads behave;
* listings show one level in order with kinds and sizes; a missing directory lists as empty or is
  not-found; deletes are final and tolerant; directories follow the declared capability, and a
  failed write creates no directory;
* no temporary object is ever visible; the durability barrier succeeds; the location can be
  prepared; the declared capabilities are enough to hold a repository.

To test a new backend, give `contract::run` a function that builds a fresh, empty instance (and a
guard that keeps a temporary directory or a test server alive):

```rust
#[test]
fn my_backend_passes_the_contract() {
    vaultsnap_core::storage::contract::run(&|| {
        Fixture::keeping(Arc::new(MyBackend::connect(test_bucket())), test_bucket_guard())
    });
}
```

The contract is run, in this repository, against the local directory, the in-memory backend, and
the in-memory backend behind a retry layer over a storage that fails every fourth call (alternately
losing the request and losing the answer); a backend that lets `Create` overwrite is shown to fail
it (`src/storage/contract.rs`). Beyond the contract, `tests/storage.rs` runs a whole repository
(init, two backups, restore byte for byte, full check, prune, check and restore again) on each of
those, copies a repository object by object from a directory to memory and back and compares the
bytes, and checks that weak backends are refused with nothing written.

## The S3 backend: design and what was built

What an `S3Backend` has to do, so that the rest needs no change, and where the implementation
(`storage/s3/`) departs from or settles it. The module has three layers: `S3Backend` (this
mapping, the request limit, the conditional-write check), the narrow `ObjectApi` trait (one method
per S3 call, with a classified error), and two implementations of that trait: the AWS SDK adapter
(`sdk.rs`, the only code that uses the network) and `FakeS3` (`fake.rs`, an in-memory service that
can page, fail on a script, ignore `If-None-Match`, and count calls). The contract runs against
the backend on the fake, so the mapping is tested without a network.

1. **Keys.** Object key = configured prefix + name. Nothing else is stored: no directory marker
   objects (`create_dir` is a no-op), no sidecar files.
2. **Reads.** `get` checks `Content-Length` against `max_len` before reading the body, and returns
   the ETag from the same response as the body, so the version belongs to the bytes. `get_prefix`
   is a ranged `GET`, with the total from `Content-Range`. Responses are read fully or the call
   fails: a short body is a transient error.
3. **Listing.** `ListObjectsV2` with `prefix = key_prefix + dir + "/"` and `delimiter = "/"`;
   common prefixes become directories; follow continuation tokens until the end; sort by name. A
   `data/` listing is 256 shard listings, and each can be large, so memory is the entries of one
   level.
4. **Writes.** `Create` is `If-None-Match: *`; `Update` is `If-Match`; `Overwrite` is a plain
   `PUT`. A `412` is `AlreadyExists` or `PreconditionFailed`. A `409 ConditionalRequestConflict`
   (a concurrent conditional write on the same key) is **transient**: retry it, the retry then gets
   its `412` or its success. Objects above the single-request threshold go through multipart
   upload; `CompleteMultipartUpload` accepts the same conditional headers, so `Create` stays
   exclusive, and an abandoned upload must be aborted so it does not leave billed parts.
   **Built without multipart:** every object is one `PUT` (chunk objects are at most the maximum
   chunk size plus a few dozen bytes, and a snapshot file is capped at 1 GiB, far below the 5 GiB
   a single `PUT` allows), so there are no abandoned uploads to abort.
5. **Capabilities** are decided by probing when connecting (see above), never by the store's name.
6. **Errors** map to `NotFound` (`404`, `NoSuchKey`), `Transient` (connection errors, timeouts,
   `500`, `502`, `503`, `SlowDown`, `409` conflicts, incomplete bodies), `Permission` (`401`,
   `403`), `Other`. The backend sets timeouts on every request; the retry layer supplies the
   backoff.
7. **Concurrency.** `backup` and `check` call the backend from several threads, so the client
   must be shared safely and its connection pool bounded. `--jobs` bounds the parallelism.
8. **Credentials** come from the environment or the provider's usual configuration, never from the
   command line (the rule that keeps the passphrase off it applies to them too), never appear in a
   `Debug` output or a log, and are the one secret VaultSnap's own encryption does not cover. The
   repository's contents are encrypted before they reach the backend; their sizes, the number of
   objects, and the times of writes are visible to the store ([`SECURITY.md`](SECURITY.md)).
9. **Tests.** The contract against a store in a container on a loopback port (21100 to 21199 in this
   project). Running it is the acceptance test for the capabilities the backend declares.
   **Built:** `tests/s3_minio.rs` runs whole-repository scenarios against MinIO, and **the whole
   contract** (`scripts/minio-local.sh`, [`S3.md`](S3.md)). Running the contract against a real
   server found that MinIO's `If-Match` creates a missing object, which is why the probe tests
   that case.

Costs to design around: every `HEAD` and `LIST` is a billed request, so the repository should not
`stat` what it can learn from one listing; `prune` and `check` already list once per shard.

## What is and is not verified

The contract (22 cases, including the racing creators and updaters and readers against a
replacement) passes on the directory backend, the in-memory backend, the in-memory backend behind
retries over a storage that fails every fourth call, the S3 backend on the in-process fake (with small
listing pages), and the S3 backend on a real MinIO server through the AWS SDK. The retry logic, the
whole-repository scenarios of `tests/storage.rs` (including moving a repository between backends
byte for byte), and the crash tests pass on macOS ([`RELIABILITY.md`](RELIABILITY.md)).

Not verified: the S3 mappings against AWS S3 itself (the real server was MinIO, built from source, over
plain HTTP, and it deviates from S3 in the one way described above), other S3-compatible services,
and any of this on Linux or Windows.
