//! What a task does. One of these runs in the `guesthouse-task` process, in the slot of its VM, while
//! the app watches the small file it leaves.

mod amd_files;
mod amd_package;
mod amd_start;
mod api_stage;
mod desktop;
mod diagnose;
mod display;
mod driver_store;
mod gpu_check;
mod guest_files;
mod guest_upload;
mod hardware_desktop;
mod identity;
mod identity_restart;
mod identity_switch;
mod install;
mod intel_device;
mod intel_files;
mod late_watch;
mod macos;
mod macos_install;
mod nvme_move;
mod own_linux;
mod remote_access;
mod ubuntu;
mod window_speed;

use amd_files::{bring_amd_driver_up_to_date, complete_host_driver};
use amd_package::register_amd_package;
pub use desktop::{apply_share, boots, rebooted};
use desktop::{note_boot, set_up_desktop, wait_for_desktop};
pub use diagnose::{API_FILE, GATHERED_FOLDER, PROBE_FILE, REPORT_FILE};
use gpu_check::{Gpu, confirm_gpu};
use guest_files::{
    place_amd_runtime, place_driver_files, place_nvapi_wrapper, place_profile_store, place_system_files,
    write_device_key,
};
use identity::set_up_identity;
use identity_restart::restart_after_switch;
use identity_switch::{new_vm_needed, stop_for_new_identity};
pub use install::{
    INSTALL_STEPS, answer_disc_path, answer_file, install, install_step, make_answer_disc, wait_for_setup,
};
use intel_files::complete_intel_driver;
use remote_access::apply_remote_access;
use window_speed::apply_window_speed;

use crate::context::Context;
use crate::disk_image::disk_size_gib;
use crate::failure::{Failure, Result};
use crate::gpu_identity::{PciIdentity, read_state};
use crate::gpu_maker::GpuMaker;
use crate::guest_memory::is_no_room;
use crate::host::{GIB, cpu_list, fits_now, ram_limit_for, smaller_start, vcache_cpus};
use crate::kept_logs;
use crate::library::MAX_DISK_GIB;
use crate::log::{clock_text, write_log};
use crate::machine::{Kind, Machine, load};
use crate::monitor::Command as Qmp;
use crate::network_settings::other_machines;
use crate::processes::qemu_pid;
use crate::screen_gpu::setup_card;
use crate::settings::max_vram_gib_for;
use crate::snapshots::{delete_snapshot, go_back, take_snapshot};
use crate::start_checks::{card_mates, start_problems, video_cap};
use crate::tasks::{TaskRecord, write_task};
use crate::text::shell_quote;
use crate::vm_args::{Boot, VmPlan};
use crate::vm_host::{Stopped, VmHost};
use rustix::fs::{FlockOperation, flock};
use rustix::process::{Pid, Signal, kill_process, kill_process_group};
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const MEMORY_WAIT: Duration = Duration::from_secs(20);
const LOCK_FILE: &str = "guesthouse.lock";
const STOP_WAIT_SECONDS: u64 = 100;
/// A guest that failed to start gets less time than one being shut down by choice.
const FAILED_START_STOP_SECONDS: u64 = 30;
const SIGTERM_WAIT_SECONDS: u64 = 20;
const SIGKILL_WAIT_SECONDS: u64 = 10;
const FORCE_POLL: Duration = Duration::from_millis(200);
pub(crate) const KEY_POLL: Duration = Duration::from_millis(500);

/// The running side of a task: reports steps, and holds what the actions need.
pub struct Task<'a> {
    pub context: &'a Context,
    pub host: &'a dyn VmHost,
    pub action: String,
    pub machine: Machine,
    pub error: String,
    /// The RAM a GPU machine starts with this time, when less than its own is free. The machine keeps
    /// its own, so the next start asks for all of it again.
    pub ram_this_run: Option<u32>,
    /// The desktop on the card is still to be watched once the start is reported done.
    watch_after_start: bool,
}

impl<'a> Task<'a> {
    pub fn new(context: &'a Context, host: &'a dyn VmHost, action: &str, machine: Machine) -> Self {
        Self {
            context,
            host,
            action: action.to_owned(),
            machine,
            error: String::new(),
            ram_this_run: None,
            watch_after_start: false,
        }
    }

    /// The machine as it starts this time.
    fn machine_this_run(&self) -> Machine {
        let mut machine = self.machine.clone();
        machine.ram_gib = self.ram_this_run.unwrap_or(machine.ram_gib);
        machine
    }

    fn record(&self, pid: u32, step: &str) -> TaskRecord {
        TaskRecord {
            action: self.action.clone(),
            machine: self.machine.id.clone(),
            pid,
            step: step.to_owned(),
            error: self.error.clone(),
        }
    }

    pub fn step(&self, text: &str) {
        println!("{} {} {text}", clock_text(), self.action);
        // A step that can't be written costs the user a line of progress, and the task goes on.
        let _ = write_task(self.context, None, &self.record(std::process::id(), text));
    }

    fn finish(&self) {
        let _ = write_task(self.context, None, &self.record(0, ""));
    }

    fn pid(&self) -> Option<u32> {
        qemu_pid(self.context, None)
    }

    fn run_file(&self, name: &str) -> PathBuf {
        self.context.run_folder(None).join(name)
    }

    fn ask_guest(&self, script: &str, seconds: u64) -> String {
        self.context.ask_guest(&self.machine, script, seconds, None)
    }

    fn sleep(&self, time: Duration) {
        self.context.clock.sleep(time);
    }

    fn now(&self) -> Duration {
        self.context.clock.now()
    }
}

pub use macos::start_refusal as macos_start_refusal;

/// The plan for this machine, in this task's slot, as things stand on the PC now.
pub fn plan_for(context: &Context, machine: &Machine, boot: Boot) -> VmPlan {
    let mates = card_mates(machine, &other_machines(context), &setup_card(context));
    let cap = video_cap(machine, &mates, max_vram_gib_for(context, machine));
    let mut plan = VmPlan::new(context, machine, boot, context.slot, cap);
    if machine.vcache_pin {
        let pin = cpu_list(&vcache_cpus(&context.cpus));
        plan.pin = Some(pin.clone());
        if crate::placement::on() {
            let threads = usize::try_from(machine.cores).unwrap_or(0);
            plan.placement = crate::placement::plan(context, &pin, threads);
        }
    }
    plan
}

/// A VM's memory comes back a few seconds after it stops, once the daemon has really gone. If it's
/// still short after that, the start says so.
fn wait_for_memory(task: &Task) {
    let end = task.now() + MEMORY_WAIT;
    let ram = u64::from(task.machine_this_run().ram_gib) * GIB;
    while fits_now(task.context, ram).is_some() && task.now() < end {
        task.sleep(KEY_POLL);
    }
}

pub fn start(task: &mut Task) -> Result<()> {
    if task.machine.kind == Kind::Linux {
        return ubuntu::start(task);
    }
    if task.machine.own_linux_gpu() {
        return own_linux::start(task);
    }
    if task.machine.macos() {
        return macos::start(task);
    }
    let machine = task.machine.clone();
    if machine.has_gpu() {
        wait_for_memory(task);
    }
    if let Some(problem) = start_problems(task.context, &machine)?.into_iter().next() {
        return Err(Failure(problem));
    }
    if machine.has_gpu()
        && let Some(short) = fits_now(task.context, u64::from(machine.ram_gib) * GIB)
    {
        // Less than it wants is better than not starting at all.
        task.ram_this_run = Some(smaller_start(task.context, machine.ram_gib).ok_or(Failure(short))?);
    }
    if !machine.has_gpu() {
        task.step("Starting the VM");
        let plan = plan_for(task.context, &machine, Boot::Plain { iso: machine.iso.clone() });
        return task.host.start(&plan, &machine);
    }
    boot(task)?;
    if let Some(gib) = task.ram_this_run {
        let note = smaller_start_note(gib, machine.ram_gib);
        task.error = [note.as_str(), &task.error].join(" ").trim().to_owned();
    }
    Ok(())
}

/// What a machine that started with less RAM than its own is told.
pub fn smaller_start_note(gib: u32, own: u32) -> String {
    format!(
        "Only room for {gib} GiB of RAM, so Windows has {gib} GiB this time, not {own}. Close some programs \
         before the next start to give it all {own}."
    )
}

/// Starts a GPU machine. One that fails leaves nothing running, and says why.
fn boot(task: &mut Task) -> Result<()> {
    task.step("Starting the VM");
    let shown = start_gpu_vm(task)?;
    let ready = get_ready(task, shown);
    if ready.is_err() {
        turn_off_after_failed_start(task);
    }
    ready
}

/// Starts QEMU and the daemon, and says which identity the render device has in this VM. A machine
/// whose first start on NVMe fails goes back to SATA and starts again there: see [`nvme_move`].
fn start_gpu_vm(task: &mut Task) -> Result<PciIdentity> {
    let started = start_gpu_vm_once(task);
    let Err(failure) = &started else {
        // The agent answered, so Windows booted from the disk it was given.
        nvme_move::passed(task);
        return started;
    };
    if !nvme_move::on_trial(task.context, &task.machine) {
        return started;
    }
    nvme_move::give_up(task, &failure.0)?;
    wait_for_memory(task);
    let shown = start_gpu_vm_once(task)?;
    task.error = [nvme_move::BACK_ON_SATA, &task.error].join(" ").trim().to_owned();
    Ok(shown)
}

fn start_gpu_vm_once(task: &mut Task) -> Result<PciIdentity> {
    let machine = task.machine_this_run();
    let plan = plan_for(task.context, &machine, Boot::Gpu);
    let started = task.host.start(&plan, &machine);
    if started.is_err() && task.pid().is_none() {
        // The daemon and shared memory are still there.
        task.host.release()?;
    }
    started.map(|()| plan.render_identity)
}

/// The guest has answered, so this takes it from there to a desktop someone can use.
fn get_ready(task: &mut Task, shown: PciIdentity) -> Result<()> {
    intel_device::switch_back_on(task, shown)?;
    let picture_first = hardware_desktop::picture_first(task);
    // A guest left running without its GPU stays as it came up, for the reports it's kept for.
    if get_gpu_ready_on_this_desktop(task, picture_first)? != Gpu::Working {
        return Ok(());
    }
    let new_identity = new_vm_needed(shown, &read_state(task.context, &task.machine), &task.machine);
    let to_nvme = nvme_move::ready_windows(task)?;
    if new_identity || to_nvme {
        start_again_in_new_vm(task, new_identity)?;
        if get_gpu_ready_on_this_desktop(task, picture_first)? != Gpu::Working {
            return Ok(());
        }
    }
    intel_device::switch_off_for_software_desktop(task, shown)?;
    set_up_desktop(task)?;
    if let Some(why) = hardware_desktop::no_picture(task) {
        return start_again_on_software_desktop(task, &why).map(|_| ());
    }
    finish_on_desktop(task)?;
    // The picture is up, so the start is done, and the watch goes on behind it.
    task.watch_after_start = picture_first && hardware_desktop::watched(task);
    Ok(())
}

/// The settings and the card maker's files, which are all asked of the guest. A VM that has gone by
/// now, as one shut down from Windows' own menu while an Intel desktop was watched has, leaves no
/// guest to ask: the start says how it went instead of what the guest didn't answer.
fn finish_on_desktop(task: &mut Task) -> Result<()> {
    if task.pid().is_none() {
        let slot = task.context.slot.number();
        task.error =
            crate::stopped::said_for_a_stop(task.context, &task.machine, slot).unwrap_or_default().to_owned();
        return Ok(());
    }
    apply_window_speed(task)?;
    apply_remote_access(task)?;
    place_files_for_maker(task)
}

/// What the guest is given for its card, by the card's maker. Each maker's files and settings are for
/// its own driver only.
fn place_files_for_maker(task: &mut Task) -> Result<()> {
    match task.machine.windows_maker() {
        Some(GpuMaker::Nvidia) => {
            place_driver_files(task)?;
            place_nvapi_wrapper(task)?;
            place_profile_store(task)
        }
        Some(GpuMaker::Amd) => {
            bring_amd_driver_up_to_date(task)?;
            place_amd_runtime(task)?;
            register_amd_package(task)
        }
        // Intel's support is new, so there is no earlier layout of its driver to bring up to date. The
        // guest has the loaders from install, and this puts back any it lacks. Another maker's files
        // would put a driver in the guest that isn't its card's.
        Some(GpuMaker::Intel) => {
            write_device_key(task)?;
            place_system_files(task)
        }
        None => Ok(()),
    }
}

/// Up to the point where it's known which identity the VM should show, before any work on a desktop
/// that might be about to go.
fn get_gpu_ready(task: &mut Task) -> Result<Gpu> {
    task.step("Waiting for the desktop");
    if !wait_for_desktop(task) {
        return Err(Failure::new("Windows started, but its desktop never came up."));
    }
    // Before the check, as a guest missing a file of Intel's driver has no GPU until it has the file.
    complete_intel_driver(task)?;
    if confirm_gpu(task)? == Gpu::Missing || restart_after_switch(task)? == Gpu::Missing {
        return Ok(Gpu::Missing);
    }
    set_up_identity(task);
    Ok(Gpu::Working)
}

/// [`get_gpu_ready`], and for a desktop on an Intel card, a watch until it has stayed up, unless
/// this start shows its picture first and is watched after. One that
/// failed is started again on the software desktop, and what that start found is then the answer.
fn get_gpu_ready_on_this_desktop(task: &mut Task, picture_first: bool) -> Result<Gpu> {
    let gpu = get_gpu_ready(task)?;
    let Some(why) = hardware_desktop::failed(task, gpu == Gpu::Missing, picture_first) else {
        return Ok(gpu);
    };
    start_again_on_software_desktop(task, &why)
}

/// A desktop on an Intel card that failed, for `why`: the machine goes to the software desktop and
/// is started again on it, and what that start found is then the answer.
fn start_again_on_software_desktop(task: &mut Task, why: &str) -> Result<Gpu> {
    let told = hardware_desktop::fall_back_and_stop(task, why)?;
    wait_for_memory(task);
    task.error.clear();
    let shown = start_gpu_vm(task)?;
    get_ready(task, shown)?;
    task.error = if task.error.is_empty() { told } else { format!("{told} {}", task.error) };
    Ok(Gpu::StartedAgain)
}

/// The same start goes on in a new VM, which shows the card maker's number if `new_identity`, and has
/// the disk on NVMe if the machine is now there. What the first one said is about a VM that's gone.
fn start_again_in_new_vm(task: &mut Task, new_identity: bool) -> Result<()> {
    if new_identity {
        stop_for_new_identity(task)?;
    } else {
        task.step(nvme_move::MOVING);
        task.host.stop(&task.machine, STOP_WAIT_SECONDS)?;
    }
    wait_for_memory(task);
    task.error.clear();
    start_gpu_vm(task).map(|_| ())
}

/// Asks Windows to go, then makes sure it does. The start has already failed, and this can only add to
/// that, so what goes wrong here is logged and the first reason stays.
fn turn_off_after_failed_start(task: &mut Task) {
    task.step("Turning off the VM");
    if task.host.stop(&task.machine, FAILED_START_STOP_SECONDS).is_ok() {
        return;
    }
    if let Err(failure) = force(task, None) {
        let _ = write_log(task.context, &format!("after a failed start: {}", failure.0));
    }
}

/// Starts what has just been installed, if it fits. An install that worked is good news, and a full
/// PC doesn't change that: the machine stays Off with Start ready.
pub(super) fn start_if_there_is_room(task: &mut Task, what: &str) -> Result<()> {
    let wanted = u64::from(task.machine.ram_gib);
    let limit = ram_limit_for(task.context, &task.machine)?;
    let started = if wanted > limit { Err(Failure::new("")) } else { start(task) };
    match started {
        Err(failure) if wanted > limit || is_no_room(&failure) => {
            task.error = format!("{what} is installed. There isn't room to start it right now.");
            Ok(())
        }
        other => other,
    }
}

fn after_reboot(task: &mut Task) -> Result<()> {
    // Before the wait, so this doesn't start again while it runs.
    note_boot(task)?;
    task.step("Waiting for the desktop");
    if !wait_for_desktop(task) {
        return Err(Failure::new("Windows restarted, but its desktop never came back."));
    }
    set_up_desktop(task)
}

/// macOS takes no request to shut down at its login screen, by key or by power button, and holds one
/// up while it asks about a document that isn't saved.
const MACOS_TURNED_OFF: &str = "macOS did not shut down, so the VM was turned off. It only shuts down \
                                when asked once someone is logged in, and waits if a document isn't saved.";

fn stop(task: &mut Task) -> Result<()> {
    let gpu = task.machine.has_gpu();
    task.step(if gpu { "Shutting down Windows" } else { "Shutting down" });
    if task.host.stop(&task.machine, STOP_WAIT_SECONDS)? == Stopped::TurnedOff {
        let who = if gpu { "Windows" } else { "The guest" };
        task.error = if task.machine.macos() {
            MACOS_TURNED_OFF.to_owned()
        } else {
            format!("{who} did not shut down in time, so the VM was turned off.")
        };
    }
    Ok(())
}

fn signal(pid: u32, sent: Signal) {
    if let Some(pid) = Pid::from_raw(pid as i32) {
        let _ = kill_process(pid, sent);
    }
}

/// Takes over from another task, if given one. For the guest this is a power cut.
fn force(task: &mut Task, other: Option<&str>) -> Result<()> {
    if let Some(pid) = other.and_then(|text| text.parse().ok()).and_then(Pid::from_raw) {
        let _ = kill_process_group(pid, Signal::TERM);
    }
    task.step("Turning off the VM");
    // SIGTERM lets QEMU write out its disk cache, and then nothing is left to stop.
    for (sent, seconds) in [(Signal::TERM, SIGTERM_WAIT_SECONDS), (Signal::KILL, SIGKILL_WAIT_SECONDS)] {
        let Some(pid) = task.pid() else { break };
        signal(pid, sent);
        let end = task.now() + Duration::from_secs(seconds);
        while task.pid().is_some() && task.now() < end {
            task.sleep(FORCE_POLL);
        }
    }
    if task.pid().is_some() {
        return Err(Failure::new("QEMU will not stop, even when killed."));
    }
    if task.machine.has_gpu() { task.host.release() } else { Ok(()) }
}

fn clean_up(task: &mut Task) -> Result<()> {
    task.step("Cleaning up after the VM");
    task.host.release()
}

/// For a guest left paused.
fn resume(task: &mut Task) -> Result<()> {
    task.step("Resuming the VM");
    task.context.ask_monitor(None, &Qmp::bare("cont"));
    task.error = if task.context.run_state(None).as_deref() == Some("running") {
        "The VM had been left paused. It is running again."
    } else {
        "The VM is paused and would not resume."
    }
    .to_owned();
    Ok(())
}

pub fn grow(task: &mut Task, gib: u32) -> Result<()> {
    if task.pid().is_some() {
        return Err(Failure::new("The disk can only grow while the VM is off."));
    }
    let size = disk_size_gib(&task.machine.disk)?;
    if f64::from(gib) <= size {
        return Err(Failure(format!("The disk is already {size} GiB, and it can only grow.")));
    }
    if gib > MAX_DISK_GIB {
        return Err(Failure(format!("The largest disk here is {MAX_DISK_GIB} GiB.")));
    }
    task.step(&format!("Growing the disk to {gib} GiB"));
    let result = Command::new("qemu-img")
        .args(["resize", "-f", "qcow2", &task.machine.disk, &format!("{gib}G")])
        .output()?;
    if !result.status.success() {
        let said = String::from_utf8_lossy(&result.stderr);
        return Err(Failure(format!("The disk did not grow: {}", said.trim())));
    }
    Ok(())
}

fn run_action(task: &mut Task, arguments: &[String]) -> Result<()> {
    let first = arguments.first().map(String::as_str);
    let text = first.unwrap_or_default();
    match task.action.as_str() {
        "start" => start(task),
        "stop" => stop(task),
        "force" => force(task, first),
        "grow" => grow(task, text.parse().map_err(|_| Failure::new("That is not a size."))?),
        "resume" => resume(task),
        "dxdiag" | "api-check" => diagnose::dxdiag(task),
        "clean_up" => clean_up(task),
        "after_reboot" => after_reboot(task),
        "apply_display" => {
            display::apply_display(task);
            Ok(())
        }
        "install_display" => {
            display::install_display(task);
            Ok(())
        }
        crate::remote_access::APPLY_TASK => remote_access::apply_change(task),
        "install" if task.machine.macos() => macos_install::install(task),
        "install" if task.machine.kind == Kind::Linux => ubuntu::install(task),
        "install" => install(task),
        "take_snapshot" => take_snapshot(task, text),
        "go_back" => go_back(task, text),
        "delete_snapshot" => delete_snapshot(task, text),
        other => Err(Failure(format!("There is no action called {}.", shell_quote(other)))),
    }
}

fn take_lock(context: &Context) -> Result<File> {
    // A task can be the first thing to run on a new install, with no run folder yet.
    context.make_run_folder(None)?;
    let lock = File::create(context.run_folder(None).join(LOCK_FILE))?;
    flock(&lock, FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| Failure::new("Something else is still in progress."))?;
    Ok(lock)
}

/// The task program's whole life: runs the action, and leaves what happened for the app to read.
/// The error comes back too, so the program can exit with a failure when the action didn't happen.
pub fn run_task(
    context: &Context,
    host: &dyn VmHost,
    action: &str,
    machine_id: &str,
    arguments: &[String],
) -> Result<()> {
    let machine =
        match crate::machine::id_from_argument(context, machine_id).and_then(|id| load(context, &id)) {
            Ok(machine) => machine,
            Err(error) => {
                let record = TaskRecord {
                    action: action.to_owned(),
                    machine: machine_id.to_owned(),
                    pid: 0,
                    step: String::new(),
                    error: error.0.clone(),
                };
                let _ = write_task(context, None, &record);
                return Err(error);
            }
        };
    let mut task = Task::new(context, host, action, machine);
    let lock = if action == "force" { Ok(None) } else { take_lock(context).map(Some) };
    let result = lock.and_then(|_lock| run_action(&mut task, arguments));
    if let Err(error) = &result {
        task.error = error.0.clone();
    }
    task.finish();
    keep_ended_run(context, action);
    let _ = writeln!(std::io::stdout(), "{} {action} finished", clock_text());
    if result.is_ok() && task.watch_after_start {
        late_watch::watch_after_start(&mut task);
    }
    result
}

/// Once a VM has ended, its logs are copied where a reboot leaves them. A run that ended unseen is
/// kept at the next start in its slot instead.
fn keep_ended_run(context: &Context, action: &str) {
    let ending = matches!(action, "stop" | "force" | "clean_up");
    if ending
        && qemu_pid(context, None).is_none()
        && let Err(error) = kept_logs::keep_run(context, &context.run_folder(None))
    {
        let _ = write_log(context, &format!("{action}: the run's logs could not be kept: {error}"));
    }
}

#[cfg(test)]
pub(crate) mod tests;
