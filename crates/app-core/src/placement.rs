//! Where a machine's threads run. The flat mask from `taskset` puts every QEMU thread on the
//! same CPUs, so the scheduler can put two vCPUs on one core and leave another core of the
//! pinned set idle. This works out one core per vCPU, the room for the rest of QEMU's threads,
//! and applies it once the VM is up.
//!
//! The vCPU threads keep the flat mask's promise: each one is narrowed to a CPU inside it. The
//! other threads may be moved outside the pinned set, which is the point of keeping the guest's
//! threads apart: the main loop, the I/O thread, and the audio worker are the host's work, and
//! their place is wherever the guest's cores are not.
//!
//! Nothing here stops a start. A thread that won't take its mask is said and left as it is, and
//! the machine runs on the flat mask it was launched with.

use crate::context::Context;
use crate::host::cpu_list;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The room for QEMU's threads that are not vCPUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aux {
    /// The other CPUs of the cores the guest's threads hold: the guest's threads never share a
    /// CPU with each other, and the rest stay on the same cores, where the cache is.
    Siblings,
    /// The CPUs the pinned set keeps out, on the other side of the chip.
    OffPin,
}

/// Which CPU of each core a vCPU gets, on a CPU with SMT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Half {
    /// The lower-numbered CPU of each core, which sysfs lists first.
    Lower,
    /// The other CPU of each core.
    Upper,
}

/// The plan for one machine's threads: one CPU per vCPU, and one mask for the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// One CPU per vCPU, in vCPU order.
    pub vcpu: Vec<String>,
    /// The mask for every thread that is not a vCPU.
    pub aux: String,
}

/// A CPU list the way sysfs writes one: `0,16` or `0-1,3`.
fn parse_list(text: &str) -> Vec<u32> {
    let mut found = BTreeSet::new();
    for part in text.split(',') {
        let part = part.trim();
        if let Some((first, last)) = part.split_once('-') {
            let (Ok(first), Ok(last)) = (first.trim().parse::<u32>(), last.trim().parse::<u32>()) else {
                continue;
            };
            found.extend(first..=last);
        } else if let Ok(one) = part.parse() {
            found.insert(one);
        }
    }
    found.into_iter().collect()
}

/// The CPUs of the core this CPU belongs to, as sysfs lists them. A CPU whose file is missing
/// stands as a core of one, so a plan can still be made on a kernel that doesn't say.
fn sibling_cpus(root: &Path, cpu: u32) -> Vec<u32> {
    let text = std::fs::read_to_string(root.join(format!("cpu{cpu}/topology/thread_siblings_list")))
        .unwrap_or_default();
    let siblings = parse_list(&text);
    if siblings.is_empty() { vec![cpu] } else { siblings }
}

/// The CPUs of the pinned set, grouped by core: siblings together, groups in the order of their
/// first CPU, each group in order.
pub fn core_groups(pin: &[u32], root: &Path) -> Vec<Vec<u32>> {
    let wanted: BTreeSet<u32> = pin.iter().copied().collect();
    let mut groups: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for &cpu in &wanted {
        let siblings = sibling_cpus(root, cpu);
        let first = siblings.iter().copied().min().unwrap_or(cpu);
        let group = groups.entry(first).or_default();
        for sibling in siblings.iter().filter(|sibling| wanted.contains(sibling)) {
            if !group.contains(sibling) {
                group.push(*sibling);
            }
        }
    }
    groups.values_mut().for_each(|group| group.sort_unstable());
    groups.into_values().collect()
}

/// The plan for a machine of `vcpus` threads on the pinned CPUs, or nothing when the pinned set
/// has fewer cores than the guest has threads: the machine then keeps the flat mask it had
/// rather than sharing a core the plan meant for one thread.
pub fn placement(pin: &[u32], vcpus: usize, root: &Path, half: Half, aux: Aux) -> Option<Placement> {
    let groups = core_groups(pin, root);
    if vcpus == 0 || groups.len() < vcpus {
        return None;
    }
    let chosen: Vec<u32> = groups
        .iter()
        .take(vcpus)
        .map(|group| match half {
            Half::Lower => group[0],
            Half::Upper => group[group.len() - 1],
        })
        .collect();
    let rest: Vec<u32> = match aux {
        Aux::Siblings => parse_list(&cpu_list(pin)).into_iter().filter(|cpu| !chosen.contains(cpu)).collect(),
        // cpu_numbers lists them as sysfs names them, which is not in number order; the mask
        // is written as runs, so the room has to be sorted first.
        Aux::OffPin => {
            let mut rest: Vec<u32> =
                crate::host::cpu_numbers(root).into_iter().filter(|c| !pin.contains(c)).collect();
            rest.sort_unstable();
            rest
        }
    };
    // A pinned set with no CPU left over, which is a machine without SMT or one pinned
    // exactly to its thread count: the room the plan wanted is nothing, so the rest keep
    // the flat mask the launcher gave them rather than an empty set, which the kernel
    // turns down.
    let rest = if rest.is_empty() { parse_list(&cpu_list(pin)) } else { rest };
    Some(Placement { vcpu: chosen.iter().map(|cpu| cpu_list(&[*cpu])).collect(), aux: cpu_list(&rest) })
}

const HALF_VARIABLE: &str = "GUESTHOUSE_PIN_HALF";
const AUX_VARIABLE: &str = "GUESTHOUSE_PIN_AUX";
/// The switch that turns the whole family off, for a run that wants the flat mask alone.
pub const OFF_VARIABLE: &str = "GUESTHOUSE_THREAD_PINNING";
/// The mask for the daemon's thread, which is the host's work and not the guest's.
pub const DAEMON_MASK_VARIABLE: &str = "GUESTHOUSE_DAEMON_MASK";

fn shape(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|text| !text.is_empty()).map(|text| text.to_lowercase())
}

/// The plan this machine should have, from what the PC offers: the pinned CPUs, the guest's
/// thread count, and the shapes the environment asks for. Nothing when there is no pinned set,
/// or when the plan cannot give every thread its own core.
pub fn plan(context: &Context, pin: &str, vcpus: usize) -> Option<Placement> {
    let pin = parse_list(pin);
    if pin.is_empty() {
        return None;
    }
    let half = match shape(HALF_VARIABLE).as_deref() {
        Some("lower") => Half::Lower,
        // The other CPU of each core is the one a host that pins its own threads to the low
        // CPUs leaves free, and it is the plan's default.
        _ => Half::Upper,
    };
    let aux = match shape(AUX_VARIABLE).as_deref() {
        Some("siblings") => Aux::Siblings,
        _ => Aux::OffPin,
    };
    placement(&pin, vcpus, &context.cpus, half, aux)
}

/// Whether the machine's threads are pointed at their plan: off only when the variable says so.
pub fn on() -> bool {
    shape(OFF_VARIABLE).as_deref() != Some("0")
}

/// The CPUs a thread may run on. rustix keeps the unsafe call out of this crate.
fn set_cpus(pid: u32, cpus: &str) -> std::io::Result<()> {
    let mut set = rustix::thread::CpuSet::new();
    for cpu in parse_list(cpus) {
        set.set(cpu as usize);
    }
    let Some(target) = rustix::process::Pid::from_raw(pid as i32) else {
        return Err(std::io::Error::from(rustix::io::Errno::INVAL));
    };
    rustix::thread::sched_setaffinity(Some(target), &set).map_err(std::io::Error::from)
}

/// The vCPU thread this name names, if it is one: QEMU calls them `CPU 0/KVM`.
fn vcpu_index(name: &str) -> Option<usize> {
    let inner = name.strip_prefix("CPU ")?.strip_suffix("/KVM")?;
    inner.parse().ok()
}

/// What a walk set: how many threads took a vCPU's CPU, how many took the aux mask, and which
/// threads kept their CPUs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    pub vcpus: usize,
    pub aux: usize,
    pub failed: Vec<String>,
}

/// The threads of one process, as its thread table names them: the id and the name.
fn threads(context: &Context, pid: u32) -> Option<Vec<(u32, String)>> {
    let table = context.processes.join(pid.to_string()).join("task");
    let entries = std::fs::read_dir(&table).ok()?;
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Ok(tid) = entry.file_name().to_string_lossy().parse() else { continue };
        let name = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default().trim().to_owned();
        found.push((tid, name));
    }
    Some(found)
}

/// Points a machine's threads at their plan: each vCPU thread at its CPU, every other thread of
/// QEMU at the aux mask. The walk finds the vCPU threads by QEMU's own naming and the rest by
/// exclusion, as what the libraries name appears and goes with the display and the sound, and the
/// kernel cuts a name at 15 characters.
///
/// Nothing is set until every vCPU thread the plan names is there: a thread that is not a vCPU
/// name takes the aux mask, so a vCPU thread missed by the walk would be sent to the aux CPUs
/// rather than left on its own. A walk that finds fewer sets nothing at all, and the machine
/// keeps the flat mask it was launched with.
pub fn apply(context: &Context, plan: &Placement, pid: u32) -> Applied {
    let Some(threads) = threads(context, pid) else {
        return Applied { failed: vec![format!("no thread table for {pid}")], ..Default::default() };
    };
    let named = threads.iter().filter(|(_, name)| vcpu_index(name).is_some()).count();
    if named != plan.vcpu.len() {
        return Applied {
            failed: vec![format!("found {named} of {} vCPU threads, so nothing was set", plan.vcpu.len())],
            ..Default::default()
        };
    }
    let mut done = Applied::default();
    for (tid, name) in threads {
        let wanted = match vcpu_index(&name) {
            Some(index) => match plan.vcpu.get(index) {
                Some(cpus) => {
                    done.vcpus += 1;
                    cpus.clone()
                }
                None => {
                    done.failed.push(format!("vCPU {index} has no plan"));
                    continue;
                }
            },
            None => {
                done.aux += 1;
                plan.aux.clone()
            }
        };
        if let Err(error) = set_cpus(tid, &wanted) {
            done.failed.push(format!("thread {tid} ({name}) kept its CPUs: {error}"));
        }
    }
    done
}

/// Points the daemon's one thread at its mask, so the host's work keeps off the guest's cores.
pub fn apply_daemon(pid: u32) {
    let Some(mask) = shape(DAEMON_MASK_VARIABLE) else { return };
    let _ = set_cpus(pid, &mask);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::stand_in::fake_cpus;
    use crate::testing::Scratch;

    /// The pinned set of a chip with one cached CCD: that CCD's CPUs and their SMT siblings,
    /// two per core, numbered apart as sysfs numbers them on this host.
    fn rig(scratch: &Scratch) -> std::path::PathBuf {
        let sizes: Vec<u32> = [[98304u32; 8], [32768; 8], [32768; 8], [32768; 8]].concat();
        let root = fake_cpus(scratch, "cpu", &sizes);
        for cpu in 0..16u32 {
            let sibling = cpu + 16;
            scratch
                .write(&format!("cpu/cpu{cpu}/topology/thread_siblings_list"), format!("{cpu},{sibling}\n"));
            scratch.write(
                &format!("cpu/cpu{sibling}/topology/thread_siblings_list"),
                format!("{cpu},{sibling}\n"),
            );
        }
        root
    }

    const PIN: [u32; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21, 22, 23];

    #[test]
    fn the_cores_of_the_pinned_set_are_found_two_cpus_each() {
        let scratch = Scratch::new();
        let groups = core_groups(&PIN, &rig(&scratch));
        assert_eq!(groups.len(), 8);
        assert_eq!(groups[0], vec![0, 16]);
        assert_eq!(groups[7], vec![7, 23]);
    }

    #[test]
    fn one_thread_per_core_and_the_rest_off_the_guests_cores() {
        let scratch = Scratch::new();
        let plan = placement(&PIN, 8, &rig(&scratch), Half::Upper, Aux::OffPin).unwrap();
        assert_eq!(plan.vcpu, ["16", "17", "18", "19", "20", "21", "22", "23"].map(String::from));
        assert_eq!(plan.aux, "8-15,24-31");
    }

    #[test]
    fn the_siblings_shape_keeps_the_rest_on_the_same_cores() {
        let scratch = Scratch::new();
        let plan = placement(&PIN, 8, &rig(&scratch), Half::Lower, Aux::Siblings).unwrap();
        assert_eq!(plan.vcpu, ["0", "1", "2", "3", "4", "5", "6", "7"].map(String::from));
        assert_eq!(plan.aux, "16-23");
    }

    /// The invariant: a machine with more threads than the pinned set has cores keeps the flat
    /// mask it had, rather than sharing a core the plan meant for one thread.
    #[test]
    fn no_plan_when_the_guests_threads_outnumber_the_cores() {
        let scratch = Scratch::new();
        let root = rig(&scratch);
        assert!(placement(&PIN, 9, &root, Half::Upper, Aux::OffPin).is_none());
        assert!(placement(&PIN, 0, &root, Half::Upper, Aux::OffPin).is_none());
        assert!(placement(&[], 8, &root, Half::Upper, Aux::OffPin).is_none());
    }

    #[test]
    fn a_core_of_one_cpu_is_a_core() {
        let scratch = Scratch::new();
        let root = fake_cpus(&scratch, "cpu", &[98304u32, 98304, 32768, 32768]);
        let plan = placement(&[0, 1], 2, &root, Half::Upper, Aux::OffPin).unwrap();
        assert_eq!(plan.vcpu, ["0", "1"].map(String::from));
        assert_eq!(plan.aux, "2-3");
    }

    /// A pinned set with no CPU left over gives the rest the flat mask, not an empty one:
    /// an empty affinity mask is an error, and the guest's threads still have their own.
    #[test]
    fn a_pinned_set_with_no_room_left_keeps_the_flat_mask_for_the_rest() {
        let scratch = Scratch::new();
        let root = fake_cpus(&scratch, "cpu", &[98304u32, 98304, 32768, 32768]);
        let plan = placement(&[0, 1], 2, &root, Half::Upper, Aux::Siblings).unwrap();
        assert_eq!(plan.vcpu, ["0", "1"].map(String::from));
        assert_eq!(plan.aux, "0-1");
    }

    #[test]
    fn the_names_of_the_vcpu_threads_are_theirs_alone() {
        assert_eq!(vcpu_index("CPU 0/KVM"), Some(0));
        assert_eq!(vcpu_index("CPU 11/KVM"), Some(11));
        assert_eq!(vcpu_index("CPU 0/KV"), None);
        assert_eq!(vcpu_index("mon_iothread"), None);
        assert_eq!(vcpu_index("CPU 1/KVM"), Some(1));
    }

    /// The setter reaches the kernel: a thread is pointed at one CPU and reads that mask back.
    #[test]
    fn a_thread_takes_the_cpus_it_is_pointed_at() {
        // The thread reads its own mask back while it is still there: once it has run, its
        // thread table entry is gone.
        let made = std::thread::spawn(|| {
            let tid = rustix::thread::gettid();
            let done = set_cpus(tid.as_raw_pid() as u32, "0");
            let status =
                std::fs::read_to_string(format!("/proc/self/task/{}/status", tid.as_raw_pid())).unwrap();
            let mask = status
                .lines()
                .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
                .map(str::trim)
                .unwrap_or_default()
                .to_owned();
            (done.is_ok(), mask)
        });
        let (done, mask) = made.join().unwrap();
        assert!(done, "the setter reached the kernel");
        assert_eq!(mask, "0");
    }

    /// The walk sets nothing until every vCPU thread the plan names is in the thread table: a
    /// thread that isn't a vCPU name takes the aux mask, so a missed vCPU would be sent off the
    /// guest's cores instead of left on them.
    #[test]
    fn a_walk_that_finds_a_short_census_sets_nothing() {
        let scratch = Scratch::new();
        let context = crate::testing::scratch_context(&scratch);
        let plan = Placement { vcpu: vec!["16".into(), "17".into()], aux: "8-15".into() };
        // The ids are above every pid the kernel can hand out, so each set is turned down: the
        // counts are of what the walk aimed at, and the test leaves no real thread with a new mask.
        for (tid, name) in [
            (4000001u32, "CPU 0/KVM"),
            (4000002, "CPU 1/KVM"),
            (4000003, "main-loop"),
            (4000004, "mon_iothread"),
        ] {
            scratch.write(&format!("proc/9/task/{tid}/comm"), format!("{name}\n"));
        }
        let found = apply(&context, &plan, 9);
        assert_eq!(found.vcpus, 2);
        assert_eq!(found.aux, 2);
        // The ids are the test's, so the kernel turns each set down: the counts are of what the
        // walk aimed at, and the failures are said.
        assert_eq!(found.failed.len(), 4);
        let short = apply(&context, &Placement { vcpu: plan.vcpu.clone(), aux: "8-15".into() }, 99);
        assert_eq!(short.vcpus, 0);
        assert_eq!(short.aux, 0);
        assert!(short.failed[0].contains("no thread table"));
    }

    #[test]
    fn the_lists_are_read_as_sysfs_writes_them() {
        assert_eq!(parse_list("0,16"), [0, 16]);
        assert_eq!(parse_list("0-3,16-19"), [0, 1, 2, 3, 16, 17, 18, 19]);
        assert_eq!(parse_list(" 4 ,, 6-7 "), [4, 6, 7]);
        assert!(parse_list("").is_empty());
    }
}
