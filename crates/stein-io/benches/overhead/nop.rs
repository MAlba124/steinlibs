//! Nops in batches of the queue depth, the pure per-op cost of each layer. The driver has
//! no nop and the epoll queue completes one inline at push.

use io_uring::opcode;
use stein_io::queue::{Cqe, Op, Want};

use crate::rig::{queue, queue_wait, raw_ring, raw_wait};
use crate::{BenchError, Ctx, exact};

pub fn run(ctx: &mut Ctx) -> Result<(), BenchError> {
    for qd in [1usize, 32] {
        let w = format!("nop/qd{qd}");
        let ops = ctx.ops(if qd == 1 { 200_000 } else { 2_000_000 });
        raw(ctx, &w, qd, ops, false)?;
        if qd > 1 {
            raw(ctx, &w, qd, ops, true)?;
        }
        stein(ctx, &w, qd, ops, Want::Uring)?;
        stein(ctx, &w, qd, ops, Want::Epoll)?;
        ctx.flush()?;
    }
    Ok(())
}

fn raw(ctx: &mut Ctx, w: &str, qd: usize, ops: usize, waitn: bool) -> Result<(), BenchError> {
    let name = if waitn {
        "raw-uring-waitn"
    } else {
        "raw-uring"
    };
    if !ctx.wants(w, name) {
        return Ok(());
    }
    let mut ring = raw_ring(ctx.coop)?;
    ctx.add(w, name, ops, 0, move |n| {
        let batches = n.div_ceil(qd);
        for _ in 0..batches {
            let mut sq = ring.submission();
            for i in 0..qd {
                let e = opcode::Nop::new().build().user_data(i as u64);
                // SAFETY: a nop names no memory.
                unsafe { sq.push(&e) }.map_err(|_| BenchError::Setup("sq full"))?;
            }
            drop(sq);
            raw_wait(&mut ring, qd, waitn, |_, r| exact("nop", r as i64, 0))?;
        }
        Ok(batches * qd)
    });
    Ok(())
}

fn stein(ctx: &mut Ctx, w: &str, qd: usize, ops: usize, want: Want) -> Result<(), BenchError> {
    let name = match want {
        Want::Epoll => "queue-epoll",
        _ => "queue-uring",
    };
    if !ctx.wants(w, name) {
        return Ok(());
    }
    let mut q = queue(want, ctx.coop)?;
    let mut out = [Cqe::EMPTY; 64];
    ctx.add(w, name, ops, 0, move |n| {
        let batches = n.div_ceil(qd);
        for _ in 0..batches {
            for i in 0..qd {
                q.push(Op::Nop, i as u64)?;
            }
            queue_wait(&mut q, &mut out, qd, |c| exact("nop", c.res as i64, 0))?;
        }
        Ok(batches * qd)
    });
    Ok(())
}
