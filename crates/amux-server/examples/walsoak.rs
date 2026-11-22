//! WAL soak (AMUX-502): the real `Store` on a COPY of the live DB under a
//! synthetic read load with concurrent writes. Prints WAL size, read and write
//! latency and CPU. No router, no jobs: it cannot touch the fleet.
//!
//!   WALSOAK_DB=/path/copy.db WALSOAK_SECS=1800 cargo run --release -p amux-server --example walsoak
//!
//! Load (env): WALSOAK_SHORT=64 board readers; WALSOAK_LONG=2 back-to-back
//! 5-40 s statements (the board-drive/autofix shape); WALSOAK_HOLD=1 reader
//! keeping a cursor open 20 s every 2 min; writer ~0.6 MB/s of WAL.
//! AMUX_WAL_SOFT_BYTES=99999999999 shows the old behaviour (no reset).
use amux_server::db::{Store, WriteOutcome};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
fn rnd(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}
fn pct(v: &mut [u32], p: f64) -> u32 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[((v.len() - 1) as f64 * p) as usize]
}
fn cpu() -> (f64, f64) {
    // SAFETY: getrusage fills a plain struct we own.
    let ru = unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        ru
    };
    let s = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    (s(ru.ru_utime), s(ru.ru_stime))
}
fn noop() -> rusqlite::Result<WriteOutcome> {
    Ok(WriteOutcome { applied: false, events: vec![] })
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    amux_server::disable_sqlite_memstatus();
    let db = std::path::PathBuf::from(std::env::var("WALSOAK_DB")?);
    let secs = env("WALSOAK_SECS", 60);
    let store = Store::open(&db)?;
    store.write(|c| {
        c.execute_batch("CREATE TABLE IF NOT EXISTS _walsoak(id INTEGER PRIMARY KEY, b BLOB)")?;
        noop()
    })?;
    let ids: Arc<Vec<String>> = Arc::new({
        let c = store.read()?;
        let mut st = c.prepare("SELECT id FROM issues")?;
        let v = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        v
    });
    let stop = Arc::new(AtomicBool::new(false));
    let lat = Arc::new(Mutex::new(Vec::<u32>::new())); // short-read latency, 0.1 ms units
    let wlat = Arc::new(Mutex::new(Vec::<u32>::new()));
    let errs = Arc::new(AtomicU64::new(0));
    let interrupted = Arc::new(AtomicU64::new(0));
    let long_done = Arc::new(AtomicU64::new(0));
    let mut th = vec![];
    for t in 0..env("WALSOAK_SHORT", 64) {
        let (store, stop, lat, errs, ids) = (store.clone(), stop.clone(), lat.clone(), errs.clone(), ids.clone());
        th.push(std::thread::spawn(move || {
            let mut seed = 0x9E3779B97F4A7C15 ^ (t + 1);
            let mut local = vec![];
            while !stop.load(Relaxed) {
                let t0 = Instant::now();
                // One "board read": gate + open cards + a few cards in full + the newest writes.
                let r = (|| -> anyhow::Result<()> {
                    let c = store.read()?;
                    let mut st = c.prepare_cached(
                        "SELECT id, status, session, title, updated FROM issues \
                         WHERE status IN ('todo','doing','review','needsyou') AND deleted IS NULL",
                    )?;
                    std::hint::black_box(st.query_map([], |r| r.get::<_, String>(0))?.count());
                    for _ in 0..5 {
                        let id = &ids[(rnd(&mut seed) % ids.len() as u64) as usize];
                        let mut st = c.prepare_cached("SELECT length(desc) + length(coalesce(log,'')) FROM issues WHERE id = ?1")?;
                        let _: Option<i64> = st.query_row([id], |r| r.get(0)).ok();
                    }
                    let mut st =
                        c.prepare_cached("SELECT sum(length(b)) FROM (SELECT b FROM _walsoak ORDER BY id DESC LIMIT 20)")?;
                    let _: Option<i64> = st.query_row([], |r| r.get(0))?;
                    Ok(())
                })();
                if r.is_err() {
                    errs.fetch_add(1, Relaxed);
                }
                local.push((t0.elapsed().as_micros() / 100) as u32);
                if local.len() >= 256 {
                    lat.lock().unwrap().append(&mut local);
                }
                std::thread::sleep(Duration::from_millis(5 + rnd(&mut seed) % 30));
            }
            lat.lock().unwrap().append(&mut local);
        }));
    }
    for t in 0..env("WALSOAK_LONG", 2) {
        let (store, stop, interrupted, long_done) = (store.clone(), stop.clone(), interrupted.clone(), long_done.clone());
        th.push(std::thread::spawn(move || {
            let mut seed = 0xD1B54A32D192ED03 ^ (t + 1);
            let per_s = env("WALSOAK_STEPS_PER_S", 13_000_000);
            while !stop.load(Relaxed) {
                let Ok(c) = store.read() else { continue };
                // Reads `issues` first so it holds a WAL snapshot, then spins 5-40 s.
                let steps = (5 + rnd(&mut seed) % 36) * per_s;
                let r: rusqlite::Result<i64> = c.query_row(
                    "WITH RECURSIVE c(x) AS (SELECT count(*) FROM issues UNION ALL SELECT x+1 FROM c LIMIT ?1) SELECT max(x) FROM c",
                    [steps],
                    |r| r.get(0),
                );
                match r {
                    Ok(_) => long_done.fetch_add(1, Relaxed),
                    Err(_) => interrupted.fetch_add(1, Relaxed),
                };
                drop(c);
                std::thread::sleep(Duration::from_millis(rnd(&mut seed) % 2000));
            }
        }));
    }
    for _ in 0..env("WALSOAK_HOLD", 1) {
        let (store, stop) = (store.clone(), stop.clone());
        th.push(std::thread::spawn(move || {
            let nap = |tenths: u32| {
                for _ in 0..tenths {
                    if stop.load(Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            };
            while !stop.load(Relaxed) {
                if let Ok(c) = store.read() {
                    let mut st = c.prepare("SELECT id FROM issues").unwrap();
                    let mut rows = st.query([]).unwrap();
                    let _ = rows.next(); // cursor left open: snapshot pinned, nothing to interrupt
                    nap(200);
                }
                nap(1000);
            }
        }));
    }
    {
        let (store, stop, wlat) = (store.clone(), stop.clone(), wlat.clone());
        th.push(std::thread::spawn(move || {
            while !stop.load(Relaxed) {
                let t0 = Instant::now();
                let _ = store.write(|c| {
                    c.execute("INSERT INTO _walsoak(b) VALUES (randomblob(20000))", [])?;
                    c.execute("DELETE FROM _walsoak WHERE id < (SELECT max(id) - 2000 FROM _walsoak)", [])?;
                    noop()
                });
                wlat.lock().unwrap().push((t0.elapsed().as_micros() / 100) as u32);
                std::thread::sleep(Duration::from_millis(40));
            }
        }));
    }
    let wal = format!("{}-wal", db.display());
    let wal_mb = || std::fs::metadata(&wal).map(|m| m.len() >> 20).unwrap_or(0);
    let (t0, mut wal_max, c0) = (Instant::now(), 0, cpu());
    while t0.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(1));
        let w = wal_mb();
        wal_max = wal_max.max(w);
        if t0.elapsed().as_secs() % 60 == 0 {
            println!(
                "t={}s wal_mb={w} wal_max_mb={wal_max} reads={} errs={} long_done={} long_interrupted={}",
                t0.elapsed().as_secs(),
                lat.lock().unwrap().len(),
                errs.load(Relaxed),
                long_done.load(Relaxed),
                interrupted.load(Relaxed)
            );
        }
    }
    let (c1, wall) = (cpu(), t0.elapsed().as_secs_f64());
    stop.store(true, Relaxed);
    for t in th {
        let _ = t.join();
    }
    let (mut l, mut w) = (lat.lock().unwrap().clone(), wlat.lock().unwrap().clone());
    let ms = |x: u32| x as f64 / 10.0;
    println!(
        "RESULT secs={secs} wal_max_mb={wal_max} wal_end_mb={} reads={} reads_per_s={:.0} read_ms p50={} p95={} p99={} max={} \
         over_2s={} read_errs={} writes={} write_ms p95={} max={} long_done={} long_interrupted={} cpu_cores user={:.2} sys={:.2}",
        wal_mb(),
        l.len(),
        l.len() as f64 / wall,
        ms(pct(&mut l, 0.5)),
        ms(pct(&mut l, 0.95)),
        ms(pct(&mut l, 0.99)),
        ms(pct(&mut l, 1.0)),
        l.iter().filter(|x| **x > 20_000).count(),
        errs.load(Relaxed),
        w.len(),
        ms(pct(&mut w, 0.95)),
        ms(pct(&mut w, 1.0)),
        long_done.load(Relaxed),
        interrupted.load(Relaxed),
        (c1.0 - c0.0) / wall,
        (c1.1 - c0.1) / wall
    );
    Ok(())
}
