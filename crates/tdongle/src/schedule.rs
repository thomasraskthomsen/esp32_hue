//! The automation schedule: a list of times of day, each with the setting the
//! lights should get then, kept sorted by time and read as a daily cycle, so
//! the entry before the first is the last.
//!
//! When an entry's time comes, the lights are only changed if they are still
//! as the previous entry left them (nobody has touched them since), or if the
//! entry is `forced`; see [`Entry::applies`].

use heapless::Vec;

/// Most entries kept.
pub const MAX_ENTRIES: usize = 8;
/// Automation needs at least this many entries, so that an entry's
/// predecessor is another one.
pub const MIN_ENTRIES: usize = 2;

pub type Entries = Vec<Entry, MAX_ENTRIES>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Minutes since midnight, local time: 0 to 1439.
    pub minute: u16,
    /// The brightness to set, 1 to 254; `None` is off.
    pub dim: Option<u8>,
    /// Whether to set it even if the lights aren't as the previous entry left
    /// them.
    pub forced: bool,
}

impl Entry {
    /// The light level this entry sets: 0 for off, else the brightness.
    pub fn level(&self) -> u8 {
        self.dim.unwrap_or(0)
    }

    /// Whether this entry should be carried out, given the `current` level of
    /// the lights and the entry `previous` to this one.
    pub fn applies(&self, current: u8, previous: &Entry) -> bool {
        self.forced || current == previous.level()
    }
}

/// Adds `entry`, replacing the one at the same time if there is one. Fails if
/// the list is full.
pub fn set(entries: &mut Entries, entry: Entry) -> Result<(), ()> {
    match entries.binary_search_by_key(&entry.minute, |e| e.minute) {
        Ok(at) => entries[at] = entry,
        Err(at) => entries.insert(at, entry).map_err(|_| ())?,
    }
    Ok(())
}

/// Removes the entry at `minute`; whether there was one.
pub fn remove(entries: &mut Entries, minute: u16) -> bool {
    match entries.binary_search_by_key(&minute, |e| e.minute) {
        Ok(at) => {
            entries.remove(at);
            true
        }
        Err(_) => false,
    }
}

/// The index of the entry at `minute`, if there is one.
pub fn due(entries: &[Entry], minute: u16) -> Option<usize> {
    entries.binary_search_by_key(&minute, |e| e.minute).ok()
}

/// The index of the entry in effect at `minute`: the last one at or before it,
/// or the last of the day if `minute` is before the first.
pub fn current(entries: &[Entry], minute: u16) -> Option<usize> {
    match entries.partition_point(|e| e.minute <= minute) {
        0 => entries.len().checked_sub(1),
        after => Some(after - 1),
    }
}

/// The entry before the one at `index`, wrapping around from the first to the
/// last.
pub fn previous(entries: &[Entry], index: usize) -> Entry {
    entries[(index + entries.len() - 1) % entries.len()]
}

/// Reads `HH:MM` as minutes since midnight.
pub fn parse_time(text: &str) -> Option<u16> {
    let (hour, minute) = text.split_once(':')?;
    let (hour, minute) = (hour.parse::<u16>().ok()?, minute.parse::<u16>().ok()?);
    (hour < 24 && minute < 60).then_some(hour * 60 + minute)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(minute: u16, dim: Option<u8>, forced: bool) -> Entry {
        Entry { minute, dim, forced }
    }

    #[test]
    fn set_keeps_order_and_overwrites() {
        let mut entries = Entries::new();
        set(&mut entries, entry(600, Some(100), false)).unwrap();
        set(&mut entries, entry(60, None, false)).unwrap();
        set(&mut entries, entry(1200, Some(254), true)).unwrap();
        assert_eq!(entries.iter().map(|e| e.minute).collect::<std::vec::Vec<_>>(), [60, 600, 1200]);
        set(&mut entries, entry(600, Some(5), true)).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1], entry(600, Some(5), true));
    }

    #[test]
    fn full_list_refuses_new_times_but_overwrites() {
        let mut entries = Entries::new();
        for i in 0..MAX_ENTRIES as u16 {
            set(&mut entries, entry(i * 10, None, false)).unwrap();
        }
        assert!(set(&mut entries, entry(555, None, false)).is_err());
        assert!(set(&mut entries, entry(10, Some(9), false)).is_ok());
    }

    #[test]
    fn remove_and_due() {
        let mut entries = Entries::new();
        set(&mut entries, entry(60, None, false)).unwrap();
        set(&mut entries, entry(600, Some(100), false)).unwrap();
        assert_eq!(due(&entries, 600), Some(1));
        assert_eq!(due(&entries, 601), None);
        assert!(remove(&mut entries, 600));
        assert!(!remove(&mut entries, 600));
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn previous_wraps_around() {
        let mut entries = Entries::new();
        set(&mut entries, entry(60, None, false)).unwrap();
        set(&mut entries, entry(600, Some(100), false)).unwrap();
        set(&mut entries, entry(1200, Some(254), false)).unwrap();
        assert_eq!(previous(&entries, 0).minute, 1200);
        assert_eq!(previous(&entries, 1).minute, 60);
        assert_eq!(previous(&entries, 2).minute, 600);
    }

    #[test]
    fn applies_when_untouched_or_forced() {
        let evening = entry(1200, Some(100), false);
        let morning = entry(420, None, false);
        // The lights are as the evening entry left them: the morning one acts.
        assert!(morning.applies(100, &evening));
        // Someone changed them: it doesn't...
        assert!(!morning.applies(254, &evening));
        // ...unless forced.
        assert!(entry(420, None, true).applies(254, &evening));
        // Off matches an entry that said off.
        assert!(evening.applies(0, &morning));
    }

    #[test]
    fn parses_times() {
        assert_eq!(parse_time("07:30"), Some(450));
        assert_eq!(parse_time("7:05"), Some(425));
        assert_eq!(parse_time("23:59"), Some(1439));
        assert_eq!(parse_time("24:00"), None);
        assert_eq!(parse_time("12:60"), None);
        assert_eq!(parse_time("1200"), None);
    }
}
