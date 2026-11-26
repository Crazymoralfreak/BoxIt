//! How much of the machine an operation may take.
//!
//! The strong methods are strong because of their dictionaries, and every
//! thread that compresses holds one. The formats plan around a budget of
//! memory; this is where the budget comes from: the machine BoxIt runs on.

use std::num::NonZero;
use std::thread;

use boxit_core::Budget;
use sysinfo::System;

/// What an operation gets even on a machine short of free memory: less than
/// this, and the usual settings of the formats no longer fit.
const MEMORY_FLOOR: u64 = 256 * 1024 * 1024;

/// Memory for one operation on a machine with `total` bytes of memory, of
/// which `available` are free: half of the machine, and no more than three
/// quarters of what is free at the moment.
pub fn memory_for(total: u64, available: u64) -> u64 {
    if total == 0 {
        // The system does not tell: the share of a modest machine.
        return Budget::DEFAULT_MEMORY;
    }
    let half = total / 2;
    half.min(available / 4 * 3).max(MEMORY_FLOOR.min(half))
}

/// The budget of an operation on this machine. `threads` is the choice of
/// the user; 0 takes one thread per processor.
pub fn system(threads: usize) -> Budget {
    let threads = if threads == 0 {
        thread::available_parallelism().map_or(1, NonZero::get)
    } else {
        threads
    };
    let memory = match System::new() {
        Ok(mut system) => {
            system.refresh_memory();
            memory_for(system.total_memory(), system.available_memory())
        }
        Err(error) => {
            tracing::warn!("the memory of the machine is not known: {error}");
            Budget::DEFAULT_MEMORY
        }
    };
    Budget { threads, memory }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn an_idle_machine_gives_half_of_its_memory() {
        assert_eq!(memory_for(16 * GIB, 14 * GIB), 8 * GIB);
    }

    #[test]
    fn a_busy_machine_gives_most_of_what_is_free() {
        assert_eq!(memory_for(16 * GIB, 4 * GIB), 3 * GIB);
    }

    #[test]
    fn there_is_a_floor_and_it_fits_the_machine() {
        assert_eq!(memory_for(16 * GIB, 0), MEMORY_FLOOR);
        // A machine smaller than twice the floor gives half of itself.
        assert_eq!(memory_for(GIB / 4, 0), GIB / 8);
        assert_eq!(memory_for(0, 0), Budget::DEFAULT_MEMORY);
    }

    #[test]
    fn this_machine_has_memory_and_processors() {
        let budget = system(0);
        assert!(budget.threads >= 1);
        assert!(budget.memory >= 64 * 1024 * 1024);
        assert_eq!(system(3).threads, 3);
    }
}
