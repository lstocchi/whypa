//! vCPU execution loop.

use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{debug, error, info, warn};

use crate::partition::Partition;

/// Run the vCPU execution loop on the calling thread.
///
/// Keeps running until the partition signals a stop, an unrecoverable error
/// occurs, or `running` is set to `false` (e.g. by the Ctrl+C handler).
pub fn run(partition: &mut Partition, kernel_entry: u64, running: &AtomicBool) {
    match partition.verify_rip(0) {
        Ok(rip) => {
            if rip != kernel_entry {
                warn!(rip = format_args!("0x{:X}", rip),
                      expected = format_args!("0x{:X}", kernel_entry),
                      "RIP mismatch before first run");
            }
        }
        Err(e) => {
            warn!(error = %e, "Could not verify initial RIP");
        }
    }

    let mut iteration: u64 = 0;
    let mut last_rip: u64 = 0;
    let mut rip_repeat_count: u64 = 0;

    while running.load(Ordering::Relaxed) {
        iteration += 1;

        let exit_context = match partition.run_vp(0) {
            Ok(ctx) => ctx,
            Err(e) => {
                error!(error = %e, iteration, "VP run failed");
                break;
            }
        };

        let current_rip = exit_context.VpContext.Rip;

        if current_rip == last_rip {
            rip_repeat_count += 1;
            if rip_repeat_count > 1000 && rip_repeat_count % 1000 == 0 {
                warn!(rip = format_args!("0x{:X}", current_rip),
                      stuck_for = rip_repeat_count, iteration,
                      "VM appears stuck at same RIP");
            }
        } else {
            if rip_repeat_count > 100 {
                debug!(from = format_args!("0x{:X}", last_rip),
                       to = format_args!("0x{:X}", current_rip),
                       after = rip_repeat_count,
                       "Resumed from stuck RIP");
            }
            last_rip = current_rip;
            rip_repeat_count = 0;
        }

        match partition.handle_exit(0, &exit_context) {
            Ok(true) => {}
            Ok(false) => {
                info!(iteration, "VM execution stopped");
                break;
            }
            Err(e) => {
                error!(error = %e, iteration,
                       rip = format_args!("0x{:X}", current_rip),
                       "Error handling VM exit");
                break;
            }
        }

        if iteration % 100_000 == 0 {
            debug!(iteration, rip = format_args!("0x{:X}", current_rip), "VM running");
        }
    }

    if !running.load(Ordering::Relaxed) {
        info!(iteration, "VM stopped by Ctrl+C");
    }
}
