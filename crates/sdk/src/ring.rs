//! The owner's end of the keyboard's report ring.

use crate::kbdproto::{SLOTS, Shared, Slot};
use std::sync::atomic::{Ordering, fence};

/// Fewer free slots than reports; nothing was written.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Full;

/// Appends all `reports` or none. `head` is the owner's own copy of the
/// write index, advanced only on success. Returns whether the driver waits
/// for a report and so needs the event.
///
/// # Safety
/// `view` points to a whole `Shared` that lives for the call and that this
/// caller alone writes to.
pub(crate) unsafe fn publish(
    view: *mut Shared,
    head: &mut u32,
    reports: &[Slot],
) -> Result<bool, Full> {
    // SAFETY: per the contract; only the atomics are borrowed.
    let (shared_head, tail, waiting) = unsafe { (&(*view).head, &(*view).tail, &(*view).waiting) };
    let used = head.wrapping_sub(tail.load(Ordering::Acquire)) as usize;
    if SLOTS.saturating_sub(used) < reports.len() {
        return Err(Full);
    }
    // SAFETY: the slots are plain bytes the driver does not read before head
    // moves past them.
    let slots = unsafe { (&raw mut (*view).slots).cast::<Slot>() };
    let mut next = *head;
    for report in reports {
        // SAFETY: the index is below SLOTS.
        unsafe { slots.add(next as usize % SLOTS).write_volatile(*report) };
        next = next.wrapping_add(1);
    }
    shared_head.store(next, Ordering::Release);
    *head = next;
    // Pairs with the driver's barrier after it sets waiting.
    fence(Ordering::SeqCst);
    Ok(waiting.load(Ordering::Relaxed) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section() -> Box<Shared> {
        // SAFETY: all-zero is a valid Shared: atomics and bytes.
        Box::new(unsafe { std::mem::zeroed() })
    }

    fn report(tag: u8) -> Slot {
        let mut slot = [0; 32];
        slot[0] = 1;
        slot[1] = tag;
        slot
    }

    fn put(shared: &mut Shared, head: &mut u32, reports: &[Slot]) -> Result<bool, Full> {
        // SAFETY: the box is a whole Shared and is only used here.
        unsafe { publish(shared, head, reports) }
    }

    #[test]
    fn reports_land_in_order_and_head_moves_after_them() {
        let mut shared = section();
        let mut head = 0;
        assert_eq!(
            put(&mut shared, &mut head, &[report(1), report(2)]),
            Ok(false)
        );
        assert_eq!(shared.head.load(Ordering::Relaxed), 2);
        assert_eq!(shared.slots[0], report(1));
        assert_eq!(shared.slots[1], report(2));
        assert_eq!(head, 2);
    }

    #[test]
    fn the_event_is_wanted_only_while_the_driver_waits() {
        let mut shared = section();
        let mut head = 0;
        shared.waiting.store(1, Ordering::Relaxed);
        assert_eq!(put(&mut shared, &mut head, &[report(1)]), Ok(true));
    }

    #[test]
    fn a_full_ring_refuses_without_writing() {
        let mut shared = section();
        let mut head = 0;
        let many = vec![report(7); SLOTS];
        assert_eq!(put(&mut shared, &mut head, &many), Ok(false));
        assert_eq!(put(&mut shared, &mut head, &[report(9)]), Err(Full));
        assert_eq!(head, SLOTS as u32);
        assert_eq!(shared.head.load(Ordering::Relaxed), SLOTS as u32);
        assert!(shared.slots.iter().all(|slot| *slot == report(7)));
    }

    #[test]
    fn a_batch_that_does_not_fit_is_refused_whole() {
        let mut shared = section();
        let mut head = 0;
        let most = vec![report(7); SLOTS - 1];
        put(&mut shared, &mut head, &most).unwrap();
        assert_eq!(
            put(&mut shared, &mut head, &[report(1), report(2)]),
            Err(Full)
        );
        assert_eq!(head, SLOTS as u32 - 1);
        assert_eq!(shared.slots[SLOTS - 1], [0; 32]);
        assert_eq!(put(&mut shared, &mut head, &[report(3)]), Ok(false));
    }

    #[test]
    fn slots_free_up_as_the_driver_advances_tail() {
        let mut shared = section();
        let mut head = 0;
        put(&mut shared, &mut head, &vec![report(1); SLOTS]).unwrap();
        shared.tail.store(3, Ordering::Relaxed);
        assert_eq!(put(&mut shared, &mut head, &[report(2); 3]), Ok(false));
        assert_eq!(put(&mut shared, &mut head, &[report(2)]), Err(Full));
        assert_eq!(shared.slots[0], report(2));
        assert_eq!(shared.slots[3], report(1));
    }

    #[test]
    fn indices_carry_across_u32_max() {
        let mut shared = section();
        let mut head = u32::MAX - 1;
        shared.head.store(head, Ordering::Relaxed);
        shared.tail.store(head, Ordering::Relaxed);
        let batch = [report(1), report(2), report(3), report(4)];
        assert_eq!(put(&mut shared, &mut head, &batch), Ok(false));
        assert_eq!(head, 2);
        assert_eq!(shared.head.load(Ordering::Relaxed), 2);
        // 2^32 is a multiple of 64, so the slot index continues unbroken.
        assert_eq!(shared.slots[62], report(1));
        assert_eq!(shared.slots[63], report(2));
        assert_eq!(shared.slots[0], report(3));
        assert_eq!(shared.slots[1], report(4));
        shared.tail.store(2, Ordering::Relaxed);
        assert_eq!(
            put(&mut shared, &mut head, &vec![report(5); SLOTS]),
            Ok(false)
        );
    }
}
