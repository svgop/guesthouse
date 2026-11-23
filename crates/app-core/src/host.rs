//! What this PC can give a machine: its cores, its memory and its disk.

use crate::context::Context;
use crate::failure::Result;
use crate::guest_memory::NO_ROOM_WORDS;
use crate::held_memory::used_by_others;
use crate::machine::{MIN_RAM_GIB, Machine, sorted_names};
use guesthouse_daemon::slot::Slot;
use rustix::fs::statvfs;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

pub const GIB: u64 = 1 << 30;
/// What a GPU guest's IO space is counted at in the PC's RAM. It fills only as far as the guest's
/// locked allocations go, which on Windows desktops was 130 to 270 MiB. Games can lock more, so this
/// keeps several times that.
pub const IO_SPACE_GIB: u64 = 2;
/// The host keeps a share of its RAM for itself, never less than the least and never more than the
/// most. A guest's RAM is claimed whole and the GPU pins what it uses, so swap can't make room later.
const HOST_SHARE_DIVISOR: u64 = 4;
/// A desktop with a browser and Steam open, and the viewer and the daemon beside them.
pub const HOST_KEEPS_LEAST_GIB: u64 = 6;
/// What the host can get by on, with little open beside the viewer and the daemon. It keeps this little
/// only when keeping its share would leave no room for a GPU machine of the least size, and the start
/// still checks what is free then.
pub const HOST_KEEPS_FLOOR_GIB: u64 = 3;
/// What the host always kept, which on a large PC is plenty.
pub const HOST_KEEPS_MOST_GIB: u64 = 16;
/// What a GPU guest's IO space is counted at when it starts. It's empty then, and fills only as far as
/// the guest's locked allocations go, so this is several times what a Windows desktop takes. The limit
/// on what a machine can be given still counts it at [`IO_SPACE_GIB`].
pub const IO_AT_START_GIB: u64 = 1;
/// What has to stay free when a GPU machine starts, beside its RAM and IO space, so the host isn't left
/// with nothing the moment the guest's RAM is claimed. Free swap counts toward it, as what sits idle on
/// the host can go there. On a 16 GiB PC every GiB kept here is one a program can't have open.
pub const FREE_AT_START_GIB: u64 = 1;
const KIB: u64 = 1024;

/// The numbers of `cpuN` folders.
pub(crate) fn cpu_numbers(root: &Path) -> Vec<u32> {
    sorted_names(root).iter().filter_map(|name| name.strip_prefix("cpu")?.parse().ok()).collect()
}

/// The CPUs with the larger L3, or nothing if they all have the same.
pub fn vcache_cpus(root: &Path) -> Vec<u32> {
    let mut sizes = BTreeMap::new();
    for cpu in cpu_numbers(root) {
        let text = fs::read_to_string(root.join(format!("cpu{cpu}/cache/index3/size"))).unwrap_or_default();
        if let Ok(size) = text.trim().trim_end_matches('K').parse::<u64>() {
            sizes.insert(cpu, size);
        }
    }
    let largest = sizes.values().copied().max();
    if sizes.values().collect::<BTreeSet<_>>().len() < 2 {
        return Vec::new();
    }
    sizes.iter().filter(|(_, size)| Some(**size) == largest).map(|(cpu, _)| *cpu).collect()
}

/// Writes CPUs the way taskset takes them: 0-7,16-23.
pub fn cpu_list(cpus: &[u32]) -> String {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &cpu in cpus {
        match runs.last_mut() {
            Some(run) if run.1 + 1 == cpu => run.1 = cpu,
            _ => runs.push((cpu, cpu)),
        }
    }
    let text = |(first, last): &(u32, u32)| {
        if first == last { first.to_string() } else { format!("{first}-{last}") }
    };
    runs.iter().map(text).collect::<Vec<_>>().join(",")
}

/// The most cores a machine can have: past this, vCPUs and the host would fight for them.
pub fn physical_cores(root: &Path) -> u32 {
    let mut cores = BTreeSet::new();
    for cpu in cpu_numbers(root) {
        let folder = root.join(format!("cpu{cpu}/topology"));
        let read = |name: &str| fs::read_to_string(folder.join(name)).ok();
        if let (Some(core), Some(package)) = (read("core_id"), read("physical_package_id")) {
            cores.insert((package, core));
        }
    }
    cores.len() as u32
}

/// What the host keeps of `total` bytes of RAM.
pub fn host_keeps(total: u64) -> u64 {
    (total / HOST_SHARE_DIVISOR).clamp(HOST_KEEPS_LEAST_GIB * GIB, HOST_KEEPS_MOST_GIB * GIB)
}

/// What a PC offers a GPU machine, and why, all in bytes. It comes from the PC's RAM and what the
/// other VMs hold, which only change when a VM starts or stops, so it stays the same while programs
/// come and go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RamOffer {
    pub total: u64,
    pub keeps: u64,
    /// What the VMs in the other slots hold.
    pub others: u64,
}

impl RamOffer {
    /// From the PC's RAM and what the other VMs hold.
    pub fn new(total: u64, others: u64) -> Self {
        Self { total, keeps: host_keeps(total), others }
    }

    /// The most RAM a GPU machine can have, in GiB. On a PC that's short of its share, that's still
    /// the least a machine runs on, if the host can keep its floor beside it.
    pub fn limit_gib(&self) -> u64 {
        let least = u64::from(MIN_RAM_GIB);
        let comfortable = self.after_keeping(self.keeps);
        if comfortable < least && self.tight(least) { least } else { comfortable }
    }

    /// What's left for a GPU machine, in GiB, when the host keeps `keeps` bytes.
    fn after_keeping(&self, keeps: u64) -> u64 {
        self.total.saturating_sub(keeps + IO_SPACE_GIB * GIB + self.others) / GIB
    }

    /// Whether a machine of `least` GiB fits only by the host keeping less than its share. Not while other
    /// VMs hold RAM: stopping one is the better way to make room.
    pub fn tight(&self, least: u64) -> bool {
        self.others < GIB / 2
            && self.after_keeping(self.keeps) < least
            && self.after_keeping(HOST_KEEPS_FLOOR_GIB * GIB) >= least
    }

    /// Where the RAM goes, as a sentence.
    fn reasons(&self) -> String {
        let mut text = format!(
            "This PC has {} GiB of RAM and keeps {} GiB for itself",
            self.total / GIB,
            self.keeps.div_ceil(GIB)
        );
        if self.others >= GIB / 2 {
            text += &format!(", and the VMs running hold {} GiB", self.others.div_ceil(GIB));
        }
        text + "."
    }

    /// What a RAM setting says under it, for a machine that needs at least `least` GiB.
    pub fn explanation(&self, least: u64) -> String {
        let limit = self.limit_gib();
        if self.tight(least) {
            let keeps = self.total / GIB - least - IO_SPACE_GIB;
            return format!(
                "{least} GiB, the least a GPU machine runs on. This PC has {} GiB of RAM, so it keeps only about \
                 {keeps} GiB for itself while the machine runs, not the {HOST_KEEPS_LEAST_GIB} GiB it would like.",
                self.total / GIB
            );
        }
        if limit >= least {
            return format!("Up to {limit} GiB. {}", self.reasons());
        }
        let way_out = if self.others >= GIB / 2 {
            "Stop another VM to make room."
        } else {
            "A GPU machine needs a PC with more RAM."
        };
        format!(
            "A GPU machine needs at least {least} GiB, and this PC can give one {limit} GiB. {} {way_out}",
            self.reasons()
        )
    }
}

/// How a RAM setting is offered. A machine that couldn't start isn't offered at all: the setting
/// stays at the least and says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RamField {
    pub lower: u64,
    pub upper: u64,
    pub value: u64,
    /// Whether a machine of the least size fits at all.
    pub fits: bool,
    pub subtitle: String,
}

impl RamField {
    /// All in GiB: the limit, the least the machine needs, and what it would like.
    pub fn new(limit: u64, least: u64, wanted: u64, explanation: String) -> Self {
        let fits = limit >= least;
        let upper = if fits { limit } else { least };
        Self { lower: least, upper, value: wanted.clamp(least, upper), fits, subtitle: explanation }
    }

    /// The setting for a GPU machine in `slot`.
    pub fn for_gpu(context: &Context, slot: Slot, least: u64, wanted: u64) -> Self {
        let (limit, explanation) = match context.ram_limit {
            Some(limit) => (limit, format!("Up to {limit} GiB.")),
            None => {
                let offer = ram_offer(context, slot);
                (offer.limit_gib(), offer.explanation(least))
            }
        };
        Self::new(limit, least, wanted, explanation)
    }

    /// The setting for a machine with no GPU.
    pub fn for_plain(context: &Context, least: u64, wanted: u64) -> Self {
        let limit = plain_ram_limit(context);
        Self::new(limit, least, wanted, format!("Up to {limit} GiB."))
    }
}

fn meminfo_bytes(context: &Context, field: &str) -> u64 {
    let text = fs::read_to_string(&context.meminfo).unwrap_or_default();
    let line = text.lines().find_map(|line| line.strip_prefix(field)?.strip_prefix(':'));
    let kib = line.and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok());
    kib.unwrap_or(0) * KIB
}

pub fn available_ram(context: &Context) -> u64 {
    meminfo_bytes(context, "MemAvailable")
}

pub fn total_ram(context: &Context) -> u64 {
    meminfo_bytes(context, "MemTotal")
}

/// What this PC offers a GPU machine in `slot`. Its own memory, if it's running, counts as free.
pub fn ram_offer(context: &Context, slot: Slot) -> RamOffer {
    RamOffer::new(total_ram(context), used_by_others(context, slot))
}

/// Whether `ram_bytes` of guest RAM fits in what's really free now. The page cache and the programs
/// open move that from one moment to the next, so it's only asked at a start. `None` is yes, and
/// otherwise it says what's short and what to do.
pub fn fits_now(context: &Context, ram_bytes: u64) -> Option<String> {
    if context.ram_limit.is_some_and(|limit| ram_bytes <= limit * GIB) {
        return None;
    }
    let available = available_ram(context);
    let swap = meminfo_bytes(context, "SwapFree");
    (!room_for(ram_bytes, available, swap)).then(|| short_now(ram_bytes, available))
}

/// Whether the guest's RAM and IO space fit in RAM, with what has to stay free beside them in RAM
/// or swap.
fn room_for(ram_bytes: u64, available: u64, swap: u64) -> bool {
    let claimed = ram_bytes + IO_AT_START_GIB * GIB;
    let kept = FREE_AT_START_GIB * GIB;
    available >= claimed && available + swap.min(kept) >= claimed + kept
}

/// The most RAM, in GiB, a GPU machine that wants `wanted_gib` can start with now, if that's less
/// than it wants but still enough for Windows. `None` if all of it fits, or if not even the least does.
pub fn smaller_start(context: &Context, wanted_gib: u32) -> Option<u32> {
    // All of it fits, so there's nothing smaller to offer.
    fits_now(context, u64::from(wanted_gib) * GIB)?;
    (MIN_RAM_GIB..wanted_gib).rev().find(|gib| fits_now(context, u64::from(*gib) * GIB).is_none())
}

pub fn short_now(ram_bytes: u64, available: u64) -> String {
    format!(
        "This machine needs {} GiB of RAM, and {} {NO_ROOM_WORDS} right now, with {} GiB of that kept for the \
         GPU and the PC. Close some programs, like a browser or a game, or stop another VM, and start it \
         again. Or give it less RAM.",
        ram_bytes.div_ceil(GIB),
        available / GIB,
        IO_AT_START_GIB + FREE_AT_START_GIB,
    )
}

/// The most RAM a GPU machine can have.
pub fn host_ram_limit(context: &Context) -> Result<u64> {
    match context.ram_limit {
        Some(limit) => Ok(limit),
        None => Ok(ram_offer(context, context.slot).limit_gib()),
    }
}

/// The most RAM a machine with no GPU can have: ordinary memory, which swap can back.
pub fn plain_ram_limit(context: &Context) -> u64 {
    let total = total_ram(context);
    (total.saturating_sub(host_keeps(total)) / GIB).max(1)
}

/// The most RAM this machine can have. A GPU machine that's running counts its own as free.
pub fn ram_limit_for(context: &Context, machine: &Machine) -> Result<u64> {
    if !machine.has_gpu() {
        return Ok(plain_ram_limit(context));
    }
    if let Some(limit) = context.ram_limit {
        return Ok(limit);
    }
    let slot = crate::tasks::slot_of(context, machine).unwrap_or(context.slot);
    Ok(ram_offer(context, slot).limit_gib())
}

pub fn free_gib(path: &Path) -> Result<f64> {
    let disk = statvfs(path).map_err(std::io::Error::from)?;
    Ok((disk.f_bavail * disk.f_frsize) as f64 / GIB as f64)
}

#[cfg(test)]
pub mod stand_in {
    use crate::testing::Scratch;
    use std::path::PathBuf;

    /// CPUs with these L3 sizes, half of them the SMT siblings of the rest.
    pub fn fake_cpus(scratch: &Scratch, name: &str, sizes: &[u32]) -> PathBuf {
        for (cpu, size) in sizes.iter().enumerate() {
            scratch.write(&format!("{name}/cpu{cpu}/cache/index3/size"), format!("{size}K\n"));
            scratch.write(
                &format!("{name}/cpu{cpu}/topology/core_id"),
                format!("{}\n", cpu % (sizes.len() / 2)),
            );
            scratch.write(&format!("{name}/cpu{cpu}/topology/physical_package_id"), "0\n");
        }
        std::fs::create_dir_all(scratch.path(name).join("cpufreq")).unwrap();
        scratch.path(name)
    }
}

#[cfg(test)]
mod tests {
    use super::stand_in::fake_cpus;
    use super::*;
    use crate::testing::Scratch;

    #[test]
    fn the_larger_cache_is_found_as_on_a_two_die_cpu() {
        let scratch = Scratch::new();
        let sizes: Vec<u32> = [[98304; 8], [32768; 8], [98304; 8], [32768; 8]].concat();
        let root = fake_cpus(&scratch, "cpu", &sizes);
        let expected: Vec<u32> = (0..8).chain(16..24).collect();
        assert_eq!(vcache_cpus(&root), expected);
        assert_eq!(cpu_list(&vcache_cpus(&root)), "0-7,16-23");
        assert_eq!(physical_cores(&root), 16);
    }

    #[test]
    fn with_one_cache_size_there_is_nothing_to_pin_to() {
        let scratch = Scratch::new();
        assert!(vcache_cpus(&fake_cpus(&scratch, "flat", &[32768; 4])).is_empty());
        assert_eq!((cpu_list(&[3]), cpu_list(&[0, 1, 3])), ("3".to_owned(), "0-1,3".to_owned()));
    }

    /// What `MemTotal` says on a PC with this much RAM in it, less what the firmware and the
    /// kernel keep.
    const fn sold_with(gib: u64) -> u64 {
        gib * GIB * 97 / 100
    }

    #[test]
    fn the_host_keeps_a_quarter_but_never_less_than_a_desktop_needs_or_more_than_it_did() {
        assert_eq!(host_keeps(sold_with(16)), HOST_KEEPS_LEAST_GIB * GIB);
        assert_eq!(host_keeps(32 * GIB), 8 * GIB);
        assert_eq!(host_keeps(91 * GIB), HOST_KEEPS_MOST_GIB * GIB);
    }

    #[test]
    fn what_a_pc_offers_follows_its_ram() {
        assert_eq!(RamOffer::new(sold_with(16), 0).limit_gib(), 7, "a 16 GB PC");
        assert_eq!(RamOffer::new(30 * GIB, 0).limit_gib(), 20, "issue 9's PC");
        assert_eq!(RamOffer::new(sold_with(32), 0).limit_gib(), 21);
        assert_eq!(RamOffer::new(91 * GIB, 0).limit_gib(), 73);
        assert_eq!(RamOffer::new(40 * GIB, 0).limit_gib(), 28, "the host keeps its share");
        assert_eq!(RamOffer::new(sold_with(8), 0).limit_gib(), 0);
    }

    #[test]
    fn a_low_offer_says_why_and_what_to_do() {
        let small = RamOffer::new(sold_with(8), 0).explanation(4);
        assert!(
            small.starts_with("A GPU machine needs at least 4 GiB, and this PC can give one 0 GiB."),
            "{small}"
        );
        assert!(small.contains("This PC has 7 GiB of RAM and keeps 6 GiB for itself."), "{small}");
        assert!(small.ends_with("needs a PC with more RAM."), "{small}");
        let crowded = RamOffer::new(16 * GIB, 6 * GIB).explanation(4);
        assert!(
            crowded.contains("the VMs running hold 6 GiB")
                && crowded.ends_with("Stop another VM to make room.")
        );
        let fine = RamOffer::new(30 * GIB, 0).explanation(4);
        assert!(fine.starts_with("Up to 20 GiB. This PC has 30 GiB of RAM and keeps 8 GiB"), "{fine}");
        assert!(!fine.contains("/dev/shm"), "{fine}");
    }

    #[test]
    fn a_pc_short_of_its_share_keeps_less_so_the_least_machine_fits() {
        let issue_197 = RamOffer::new(11 * GIB + GIB / 2, 0);
        assert!(issue_197.tight(4));
        assert_eq!(issue_197.limit_gib(), 4);
        let said = issue_197.explanation(4);
        assert!(said.starts_with("4 GiB, the least") && said.contains("keeps only about 5 GiB"), "{said}");
        let field = RamField::new(issue_197.limit_gib(), 4, 16, said);
        assert_eq!((field.fits, field.value), (true, 4));
        assert!(!RamOffer::new(sold_with(8), 0).tight(4), "an 8 GB PC can't keep its floor beside 4 GiB");
        assert!(!RamOffer::new(sold_with(16), 0).tight(4), "a 16 GB PC has room with its share");
        assert!(!RamOffer::new(16 * GIB, 6 * GIB).tight(4), "another VM's RAM still counts");
    }

    #[test]
    fn a_setting_never_offers_a_machine_that_cant_start() {
        let short = RamField::new(2, 4, 16, "why".into());
        assert_eq!((short.lower, short.upper, short.value, short.fits), (4, 4, 4, false));
        let roomy = RamField::new(21, 4, 16, "fine".into());
        assert_eq!((roomy.lower, roomy.upper, roomy.value, roomy.fits), (4, 21, 16, true));
        assert_eq!(RamField::new(8, 4, 16, String::new()).value, 8, "what it would like, as far as it goes");
    }

    #[test]
    fn a_gpu_setting_stays_the_same_however_little_is_free_this_second() {
        let scratch = Scratch::new();
        let mut context = crate::testing::scratch_context(&scratch);
        context.ram_limit = None;
        context.meminfo = scratch.write("meminfo", "MemTotal: 8126464 kB\nMemAvailable: 6000000 kB\n");
        let field = RamField::for_gpu(&context, crate::slots::first(), 4, 16);
        assert!(!field.fits && field.subtitle.starts_with("A GPU machine needs at least 4 GiB"), "{field:?}");
        context.meminfo = scratch.write("meminfo", "MemTotal: 31457280 kB\nMemAvailable: 1000000 kB\n");
        let field = RamField::for_gpu(&context, crate::slots::first(), 4, 16);
        assert_eq!((field.fits, field.upper, field.value), (true, 20, 16), "{field:?}");
    }

    #[test]
    fn a_start_is_checked_against_what_is_free_then() {
        let scratch = Scratch::new();
        let mut context = crate::testing::scratch_context(&scratch);
        context.ram_limit = None;
        context.meminfo = scratch.write("meminfo", "MemTotal: 31457280 kB\nMemAvailable: 26214400 kB\n");
        assert_eq!(fits_now(&context, 12 * GIB), None);
        assert_eq!(fits_now(&context, 23 * GIB), None, "25 GiB free holds 23 with 2 to spare");
        let short = fits_now(&context, 24 * GIB).unwrap();
        assert!(
            short.starts_with("This machine needs 24 GiB of RAM, and 25 GiB is free right now"),
            "{short}"
        );
        assert!(short.contains("Close some programs"), "{short}");
        assert!(crate::guest_memory::is_no_room(&crate::Failure(short)));
    }

    #[test]
    fn a_16_gib_pc_starts_6_gib_beside_7_gib_of_programs() {
        let scratch = Scratch::new();
        let mut context = crate::testing::scratch_context(&scratch);
        context.ram_limit = None;
        // 15 GiB the kernel offers, of which 7.5 GiB is free.
        context.meminfo = scratch.write("meminfo", "MemTotal: 15728640 kB\nMemAvailable: 7864320 kB\n");
        assert!(fits_now(&context, 6 * GIB).is_some(), "with no swap, 1 GiB has to stay free in RAM");
        assert_eq!(smaller_start(&context, 6), Some(5), "but 5 GiB starts");
        let with_swap = "MemTotal: 15728640 kB\nMemAvailable: 7864320 kB\nSwapFree: 4194304 kB\n";
        context.meminfo = scratch.write("meminfo", with_swap);
        assert_eq!(fits_now(&context, 6 * GIB), None, "swap holds what stays free");
        assert_eq!(smaller_start(&context, 6), None, "all of it fits");
        assert!(fits_now(&context, 7 * GIB).is_some(), "the guest's RAM itself never counts on swap");
    }

    #[test]
    fn a_start_too_short_for_the_least_is_refused() {
        let scratch = Scratch::new();
        let mut context = crate::testing::scratch_context(&scratch);
        context.ram_limit = None;
        context.meminfo = scratch.write("meminfo", "MemTotal: 15728640 kB\nMemAvailable: 5242880 kB\n");
        assert_eq!(smaller_start(&context, 6), None, "5 GiB free can't hold 4 with 2 beside it");
        assert!(fits_now(&context, 4 * GIB).is_some());
    }

    #[test]
    fn the_other_vms_count() {
        assert_eq!(RamOffer::new(91 * GIB, 17 * GIB).limit_gib(), 56);
        assert_eq!(RamOffer::new(40 * GIB, 20 * GIB).limit_gib(), 8);
    }
}
