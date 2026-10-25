//! stein-io against raw baselines of the same shape. Blocking syscalls, raw `io-uring` with
//! the ring flags the queue sets, raw epoll, then the stein queue and driver on both backends.
//!
//! `cargo bench -p stein-io --bench overhead -- [filter] [--reps N] [--scale F] [--once]
//! [--coop] [--cpu N] [--peer-cpu N] [--no-pin]`
//!
//! The filter is a substring of `workload/variant`, e.g. `disk-read` or `ping-unix/io-`,
//! or with a trailing `$` its end, so `qd32/io-uring$` leaves out `io-uring-region`.
//! `--once` runs one timed pass with no warmup and no page cache warming, for `strace -c`
//! and `perf`. Run without it first so the files exist and are cached.
//! `--coop` builds every ring with `COOP_TASKRUN` in place of `SINGLE_ISSUER` and
//! `DEFER_TASKRUN`, and the driver with `Io::with_backend` in place of `Io::single_issuer`.

mod disk;
mod net;
mod nop;
mod rig;

use std::fmt;
use std::time::Instant;

use stein_arena::counting;
use stein_io::IoError;

#[global_allocator]
static ALLOC: stein_arena::Counting = stein_arena::Counting;

#[derive(Debug)]
pub enum BenchError {
    Os(std::io::Error),
    Stein(IoError),
    /// An operation ended with an error or a short count.
    Op {
        what: &'static str,
        res: i64,
    },
    Setup(&'static str),
    Args(String),
}

impl fmt::Display for BenchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BenchError::Os(e) => write!(f, "os error {e}"),
            BenchError::Stein(e) => write!(f, "stein-io error {e}"),
            BenchError::Op { what, res } => write!(f, "{what} returned {res}"),
            BenchError::Setup(s) => write!(f, "setup failed, {s}"),
            BenchError::Args(s) => write!(f, "bad arguments, {s}"),
        }
    }
}

impl std::error::Error for BenchError {}

impl From<std::io::Error> for BenchError {
    fn from(e: std::io::Error) -> BenchError {
        BenchError::Os(e)
    }
}

impl From<IoError> for BenchError {
    fn from(e: IoError) -> BenchError {
        BenchError::Stein(e)
    }
}

/// Fails unless `res` is exactly `len`.
pub fn exact(what: &'static str, res: i64, len: usize) -> Result<(), BenchError> {
    if res == len as i64 {
        return Ok(());
    }
    Err(BenchError::Op { what, res })
}

/// Fails on a negative result. Returns it as a count.
pub fn count(what: &'static str, res: i64) -> Result<usize, BenchError> {
    if res < 0 {
        return Err(BenchError::Op { what, res });
    }
    Ok(res as usize)
}

#[derive(Clone, Copy, Debug)]
pub struct Stats {
    median: f64,
    min: f64,
    max: f64,
}

pub struct Row {
    workload: String,
    variant: &'static str,
    ns: Stats,
    bytes_per_op: u64,
    allocs_per_op: f64,
}

pub struct Ctx {
    filter: Option<String>,
    reps: usize,
    scale: f64,
    once: bool,
    pub coop: bool,
    pub peer_cpu: Option<usize>,
    rows: Vec<Row>,
    jobs: Vec<Job>,
}

impl Ctx {
    pub fn wants(&self, workload: &str, variant: &str) -> bool {
        let name = format!("{workload}/{variant}");
        match &self.filter {
            None => true,
            Some(f) => match f.strip_suffix('$') {
                Some(exact) => name.ends_with(exact),
                None => name.contains(f.as_str()),
            },
        }
    }

    /// Ops per timed pass, scaled.
    pub fn ops(&self, base: usize) -> usize {
        ((base as f64 * self.scale) as usize).max(1)
    }

    /// Queue a variant of the current workload. Its fixtures live in `body`, which runs
    /// about `n` ops and returns how many it ran. Nothing runs until [`Ctx::flush`].
    pub fn add(
        &mut self,
        workload: &str,
        variant: &'static str,
        ops: usize,
        bytes_per_op: u64,
        body: impl FnMut(usize) -> Result<usize, BenchError> + 'static,
    ) {
        if !self.wants(workload, variant) {
            return;
        }
        self.jobs.push(Job {
            workload: workload.to_string(),
            variant,
            ops,
            bytes_per_op,
            body: Box::new(body),
            ns: Vec::with_capacity(self.reps),
            allocs: 0,
            done: 0,
        });
    }

    /// Warm every queued variant, then time `reps` rounds, each running every variant once
    /// in turn, so drift on a shared machine lands on all of them alike. Allocations count
    /// on this thread inside timed passes only. Fixtures drop at the end.
    pub fn flush(&mut self) -> Result<(), BenchError> {
        let mut jobs = core::mem::take(&mut self.jobs);
        let reps = if self.once { 1 } else { self.reps };
        if !self.once {
            for j in &mut jobs {
                (j.body)((j.ops / 5).max(1))?;
            }
        }
        let turbo = read_trim(NO_TURBO);
        let mut flipped = false;
        for _ in 0..reps {
            flipped |= read_trim(NO_TURBO) != turbo;
            for j in &mut jobs {
                let before = counting::thread_count();
                let t = Instant::now();
                let n = (j.body)(j.ops)?;
                let took = t.elapsed().as_nanos() as f64;
                j.allocs += counting::thread_count() - before;
                j.done += n;
                j.ns.push(took / n.max(1) as f64);
            }
        }
        flipped |= read_trim(NO_TURBO) != turbo;
        if flipped {
            eprintln!("  turbo state changed during this workload, marked *");
        }
        for j in &mut jobs {
            j.ns.sort_by(f64::total_cmp);
            let stats = Stats {
                median: j.ns[j.ns.len() / 2],
                min: j.ns[0],
                max: j.ns[j.ns.len() - 1],
            };
            eprintln!(
                "  {:<22} {:<18} {:>10.1} ns/op  ({} ops)",
                j.workload, j.variant, stats.median, j.done
            );
            if flipped {
                j.workload.push('*');
            }
            self.rows.push(Row {
                workload: core::mem::take(&mut j.workload),
                variant: j.variant,
                ns: stats,
                bytes_per_op: j.bytes_per_op,
                allocs_per_op: j.allocs as f64 / j.done.max(1) as f64,
            });
        }
        Ok(())
    }
}

/// A queued variant and its samples.
struct Job {
    workload: String,
    variant: &'static str,
    ops: usize,
    bytes_per_op: u64,
    body: Box<dyn FnMut(usize) -> Result<usize, BenchError>>,
    ns: Vec<f64>,
    allocs: usize,
    done: usize,
}

fn is_raw(v: &str) -> bool {
    v.starts_with("raw-") || v == "blocking"
}

/// The raw baseline a stein variant runs over. The ring for ring variants, raw epoll for
/// epoll ones, else the blocking call, which is what the epoll backend makes for files.
fn same_raw<'a>(rows: &'a [Row], workload: &str, v: &str) -> Option<&'a Row> {
    let find = |name: &str| {
        rows.iter()
            .find(|r| r.workload == workload && r.variant == name)
    };
    if v.contains("uring") {
        return find("raw-uring");
    }
    if v.contains("epoll") {
        return find("raw-epoll").or_else(|| find("blocking"));
    }
    None
}

fn print_table(rows: &[Row]) {
    println!(
        "\n{:<22} {:<18} {:>9} {:>9} {:>9} {:>7} {:>7} {:>7} {:>7} {:>8}",
        "workload",
        "variant",
        "ns/op",
        "min",
        "max",
        "spread",
        "GB/s",
        "x best",
        "x same",
        "alloc/op"
    );
    let mut last = "";
    for r in rows {
        if r.workload != last && !last.is_empty() {
            println!();
        }
        last = &r.workload;
        let best = rows
            .iter()
            .filter(|o| o.workload == r.workload && is_raw(o.variant))
            .map(|o| o.ns.median)
            .fold(f64::MAX, f64::min);
        let same = same_raw(rows, &r.workload, r.variant).filter(|s| s.variant != r.variant);
        let gbs = match r.bytes_per_op {
            0 => "-".to_string(),
            b => format!("{:.2}", b as f64 / r.ns.median),
        };
        let x_best = match best {
            f64::MAX => "-".to_string(),
            b => format!("{:.3}", r.ns.median / b),
        };
        let x_same = same.map_or("-".to_string(), |s| {
            format!("{:.3}", r.ns.median / s.ns.median)
        });
        let spread = (r.ns.max - r.ns.min) / r.ns.median * 100.0;
        println!(
            "{:<22} {:<18} {:>9.1} {:>9.1} {:>9.1} {:>6.1}% {:>7} {:>7} {:>7} {:>8.3}",
            r.workload,
            r.variant,
            r.ns.median,
            r.ns.min,
            r.ns.max,
            spread,
            gbs,
            x_best,
            x_same,
            r.allocs_per_op
        );
    }
}

struct Args {
    filter: Option<String>,
    reps: usize,
    scale: f64,
    once: bool,
    coop: bool,
    cpu: Option<usize>,
    peer_cpu: Option<usize>,
}

fn value<T: std::str::FromStr>(
    it: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<T, BenchError> {
    let v = it
        .next()
        .ok_or_else(|| BenchError::Args(format!("{flag} needs a value")))?;
    v.parse()
        .map_err(|_| BenchError::Args(format!("{flag} {v} does not parse")))
}

fn args() -> Result<Args, BenchError> {
    let mut a = Args {
        filter: None,
        reps: 7,
        scale: 1.0,
        once: false,
        coop: false,
        cpu: Some(2),
        peer_cpu: Some(4),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--reps" => a.reps = value::<usize>(&mut it, "--reps")?.max(1),
            "--scale" => a.scale = value(&mut it, "--scale")?,
            "--once" => a.once = true,
            "--coop" => a.coop = true,
            "--cpu" => a.cpu = Some(value(&mut it, "--cpu")?),
            "--peer-cpu" => a.peer_cpu = Some(value(&mut it, "--peer-cpu")?),
            "--no-pin" => (a.cpu, a.peer_cpu) = (None, None),
            // Cargo passes `--bench`.
            s if s.starts_with("--") => {}
            s => a.filter = Some(s.to_string()),
        }
    }
    Ok(a)
}

/// Something on the machine may toggle turbo under load, which moves every number.
const NO_TURBO: &str = "/sys/devices/system/cpu/intel_pstate/no_turbo";

fn read_trim(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "?".to_string())
}

fn header(a: &Args) {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|m| m.trim().to_string())
        })
        .unwrap_or_else(|| "?".to_string());
    println!("cpu      {cpu}");
    println!("kernel   {}", read_trim("/proc/sys/kernel/osrelease"));
    println!(
        "governor {}, no_turbo {}",
        read_trim("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
        read_trim(NO_TURBO)
    );
    println!(
        "pinned   main {:?}, peer {:?}, reps {}, scale {}",
        a.cpu, a.peer_cpu, a.reps, a.scale
    );
    println!(
        "sizes    queue::Op {} B, queue::Cqe {} B, op::Completion {} B",
        size_of::<stein_io::queue::Op>(),
        size_of::<stein_io::queue::Cqe>(),
        size_of::<stein_io::op::Completion>()
    );
    println!(
        "rings    {}",
        if a.coop {
            "COOP_TASKRUN | TASKRUN_FLAG | SUBMIT_ALL, cq x4"
        } else {
            "SINGLE_ISSUER | DEFER_TASKRUN | TASKRUN_FLAG | SUBMIT_ALL, cq x4"
        }
    );
}

fn run() -> Result<(), BenchError> {
    let a = args()?;
    if let Some(c) = a.cpu {
        rig::pin(c)?;
    }
    header(&a);
    let mut ctx = Ctx {
        filter: a.filter.clone(),
        reps: a.reps,
        scale: a.scale,
        once: a.once,
        coop: a.coop,
        peer_cpu: a.peer_cpu,
        rows: Vec::with_capacity(512),
        jobs: Vec::with_capacity(16),
    };
    rig::report_flags(&ctx)?;
    counting::arm();
    nop::run(&mut ctx)?;
    disk::run(&mut ctx)?;
    net::run(&mut ctx)?;
    print_table(&ctx.rows);
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("overhead failed, {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
