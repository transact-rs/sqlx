use criterion::{criterion_group, criterion_main, Bencher, BenchmarkId, Criterion};
use sqlx_core::any::AnyPoolOptions;
use std::fmt::{Display, Formatter};
use std::thread;
use std::time::{Duration, Instant};
use tracing::Instrument;
use tracing_flame::FlameLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

#[derive(Debug)]
struct Input {
    threads: usize,
    tasks: usize,
    pool_size: usize,
}

impl Display for Input {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "threads: {}, tasks: {}, pool size: {}",
            self.threads, self.tasks, self.pool_size
        )
    }
}

fn bench_pool(c: &mut Criterion) {
    sqlx::any::install_default_drivers();

    let _guard = if let Ok(path) = dotenvy::var("FLAMEGRAPH_OUT") {
        let (layer, guard) = FlameLayer::with_file(&path)
            .expect(&format!("error opening path {path:?} (`FLAMEGRAPH_OUT`)"));

        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_filter(EnvFilter::from_default_env()))
            .with(layer.with_threads_collapsed(true))
            .try_init()
            .ok();

        tracing::info!("Writing flamegraph to {path:?}");

        Some(guard)
    } else {
        tracing_subscriber::fmt::try_init().ok();

        None
    };

    let database_url = dotenvy::var("DATABASE_URL").expect("DATABASE_URL must be set");

    let inputs = [
        Input {
            threads: 1,
            tasks: 2,
            pool_size: 20,
        },
        Input {
            threads: 2,
            tasks: 4,
            pool_size: 20,
        },
        Input {
            threads: 4,
            tasks: 8,
            pool_size: 20,
        },
        Input {
            threads: 8,
            tasks: 16,
            pool_size: 20,
        },
        Input {
            threads: 16,
            tasks: 32,
            pool_size: 64,
        },
        Input {
            threads: 16,
            tasks: 128,
            pool_size: 64,
        },
    ];

    let mut group = c.benchmark_group("Bench Pool");

    for input in inputs {
        group.bench_with_input(BenchmarkId::from_parameter(&input), &input, |b, i| {
            bench_pool_with(b, i, &database_url)
        });
    }

    group.finish();
}

fn bench_pool_with(b: &mut Bencher, input: &Input, database_url: &str) {
    let _span = tracing::info_span!(
        "bench_pool_with",
        threads = input.threads,
        tasks = input.tasks,
        pool_size = input.pool_size
    )
    .entered();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(input.threads)
        .build()
        .unwrap();

    let pool = runtime.block_on(async {
        AnyPoolOptions::new()
            .min_connections(input.pool_size)
            .max_connections(input.pool_size)
            .test_before_acquire(false)
            .connect(database_url)
            .await
            .expect("error connecting to pool")
    });

    for num in 1..=input.tasks {
        let pool = pool.clone();

        runtime.spawn(
            async move { while pool.acquire().await.is_ok() {} }
                .instrument(tracing::info_span!("task", num)),
        );
    }

    // Spawn the benchmark loop into the runtime so we're not accidentally including the main thread
    b.to_async(&runtime).iter_custom(|iters| {
        let pool = pool.clone();

        async move {
            tokio::spawn(
                async move {
                    let start = Instant::now();

                    for _ in 0..iters {
                        if let Err(e) = pool.acquire().await {
                            panic!("failed to acquire connection: {e:?}");
                        }
                    }

                    start.elapsed()
                }
                .instrument(tracing::info_span!("iter")),
            )
            .await
            .expect("panic in task")
        }
    });

    runtime.block_on(pool.close());
    // Give the server a second to clean up
    thread::sleep(Duration::from_millis(50));
}

criterion_group!(benches, bench_pool,);
criterion_main!(benches);
