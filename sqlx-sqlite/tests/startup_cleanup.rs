//! Kept in its own test binary because SQLite auto-extensions are process-global.

use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use futures_util::future::{select, Either};
use futures_util::{pin_mut, FutureExt};
use libsqlite3_sys::{
    sqlite3, sqlite3_api_routines, sqlite3_auto_extension, sqlite3_cancel_auto_extension,
    sqlite3_context, sqlite3_create_function_v2, sqlite3_result_null, sqlite3_value, SQLITE_OK,
    SQLITE_UTF8,
};
use sqlx::{Connection, SqliteConnection};
use sqlx_sqlite::{SqliteConnectOptions, SqliteJournalMode};
use tracing::{span::Id, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

static OBSERVER: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

struct WorkerGate {
    caller: ThreadId,
    exit_count: AtomicUsize,
    pause_at: usize,
    entered: flume::Sender<()>,
    release: flume::Receiver<()>,
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for WorkerGate {
    fn on_exit(&self, id: &Id, context: Context<'_, S>) {
        if std::thread::current().id() == self.caller
            || context
                .span(id)
                .is_none_or(|span| span.name() != "startup_cleanup")
        {
            return;
        }

        if self.exit_count.fetch_add(1, Ordering::Relaxed) + 1 == self.pause_at {
            // Hold the worker's connection reference after processing the
            // failing startup command. Otherwise dropping the caller's last
            // reference could run SQLite's destructor on either thread.
            let _ = self.entered.send(());
            // Dropping the sender also releases the worker if the test fails.
            let _ = self.release.recv_timeout(Duration::from_secs(10));
        }
    }
}

unsafe extern "C" fn scalar(context: *mut sqlite3_context, _: c_int, _: *mut *mut sqlite3_value) {
    // SAFETY: SQLite supplies a valid context for this scalar invocation.
    unsafe { sqlite3_result_null(context) };
}

unsafe extern "C" fn on_close(data: *mut c_void) {
    // SAFETY: on_open transfers exactly one boxed Arc to SQLite's destructor.
    let observer = unsafe { Box::from_raw(data.cast::<Arc<AtomicBool>>()) };
    observer.store(true, Ordering::Release);
}

unsafe extern "C" fn on_open(
    db: *mut sqlite3,
    _: *mut *mut c_char,
    _: *const sqlite3_api_routines,
) -> c_int {
    let observer = OBSERVER.lock().unwrap().as_ref().unwrap().clone();
    let data = Box::into_raw(Box::new(observer));
    // SAFETY: SQLite calls this extension with exclusive access to a valid
    // connection. It owns data, including on registration failure, and invokes
    // on_close exactly once when the registered scalar is destroyed.
    unsafe {
        sqlite3_create_function_v2(
            db,
            c"sqlx_close_observer".as_ptr(),
            0,
            SQLITE_UTF8,
            data.cast(),
            Some(scalar),
            None,
            None,
            Some(on_close),
        )
    }
}

struct AutoExtension;

impl Drop for AutoExtension {
    fn drop(&mut self) {
        // SAFETY: this removes only this test's callback, not other extensions.
        unsafe { sqlite3_cancel_auto_extension(Some(on_open)) };
        *OBSERVER.lock().unwrap() = None;
    }
}

#[test]
fn startup_errors_wait_for_connection_cleanup() {
    // SAFETY: this isolated test binary owns the callback and observer for the
    // entire lifetime of every connection it creates.
    assert_eq!(unsafe { sqlite3_auto_extension(Some(on_open)) }, SQLITE_OK);
    let _extension = AutoExtension;
    let corrupt = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(corrupt.path(), b"not a SQLite database").unwrap();
    let collation_refs = Arc::new(());
    let valid_collation = collation_refs.clone();
    let invalid_collation = collation_refs.clone();

    let cases = [
        (
            SqliteConnectOptions::new().pragma("sqlx_invalid", "("),
            "syntax error",
            1,
        ),
        (
            SqliteConnectOptions::new()
                .filename(corrupt.path())
                .journal_mode(SqliteJournalMode::Wal),
            "file is not a database",
            1,
        ),
        (
            SqliteConnectOptions::new()
                .collation("valid_collation", move |left, right| {
                    let _keep_alive = &valid_collation;
                    left.cmp(right)
                })
                .collation("invalid\0collation", move |left, right| {
                    let _keep_alive = &invalid_collation;
                    left.cmp(right)
                }),
            "invalid collation name",
            2,
        ),
    ];

    for (options, expected_error, pause_at) in cases {
        let (entered_tx, entered_rx) = flume::unbounded();
        let (release_tx, release_rx) = flume::unbounded();
        let observer = Arc::new(AtomicBool::new(false));
        let gate = WorkerGate {
            caller: std::thread::current().id(),
            exit_count: AtomicUsize::new(0),
            pause_at,
            entered: entered_tx,
            release: release_rx,
        };
        *OBSERVER.lock().unwrap() = Some(observer.clone());

        tracing::subscriber::with_default(tracing_subscriber::registry().with(gate), || {
            let _span = tracing::info_span!("startup_cleanup").entered();
            futures_executor::block_on(async {
                // On a corrupt database optimize can fail too. Cleanup must not
                // return early or replace the original initialization error.
                let options = options.optimize_on_close(true, None);
                let connect = SqliteConnection::connect_with(&options);
                let entered = entered_rx.recv_async();
                pin_mut!(connect, entered);

                match select(connect.as_mut(), entered).await {
                    Either::Left((result, _)) => {
                        panic!("connect returned before cleanup completed: {result:?}");
                    }
                    Either::Right((entered, _)) => entered.unwrap(),
                }

                // The worker still owns the connection. Error return must wait for
                // shutdown, not merely enqueue it and drop the caller's handle.
                assert!(connect.as_mut().now_or_never().is_none());
                assert!(!observer.load(Ordering::Acquire));
                release_tx.send(()).unwrap();
                let error = connect.await.unwrap_err();
                assert!(observer.load(Ordering::Acquire));
                assert!(error.to_string().contains(expected_error), "{error}");
            });
        });
    }

    assert_eq!(
        std::fs::read(corrupt.path()).unwrap(),
        b"not a SQLite database"
    );
    assert_eq!(Arc::strong_count(&collation_refs), 1);
}
