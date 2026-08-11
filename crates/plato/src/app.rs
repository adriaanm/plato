use std::fs::File;
use std::env;
use std::thread;
use std::process::Command;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use plato_core::anyhow::{Error, Context as ResultExt, format_err};
use plato_core::chrono::Local;
use plato_core::framebuffer::{Framebuffer, KoboFramebuffer1, KoboFramebuffer2, KindleFramebuffer, UpdateMode};
use plato_core::view::{View, Event, EntryId, EntryKind, ViewId, AppCmd, RenderData, RenderQueue, UpdateData};
use plato_core::view::{handle_event, process_render_queue, wait_for_all};
use plato_core::view::common::{locate, locate_by_id, transfer_notifications, overlapping_rectangle};
use plato_core::view::common::{toggle_input_history_menu, toggle_keyboard_layout_menu};
use plato_core::view::frontlight::FrontlightWindow;
use plato_core::view::menu::{Menu, MenuKind};
use plato_core::view::dictionary::Dictionary as DictionaryApp;
use plato_core::view::calculator::Calculator;
use plato_core::view::sketch::Sketch;
use plato_core::view::touch_events::TouchEvents;
use plato_core::view::rotation_values::RotationValues;
use plato_core::document::sys_info_as_html;
use plato_core::input::{DeviceEvent, PowerSource, ButtonCode, ButtonStatus, VAL_RELEASE, VAL_PRESS};
use plato_core::input::{raw_events, device_events, usb_events, display_rotate_event, button_scheme_event};
use plato_core::gesture::{GestureEvent, gesture_events};
use plato_core::helpers::{load_toml, save_toml, is_installed, suspend_outcome, SuspendOutcome};
use plato_core::settings::{ButtonScheme, Settings, SETTINGS_PATH, RotationLock, IntermKind};
use plato_core::frontlight::{Frontlight, StandardFrontlight, NaturalFrontlight, PremixedFrontlight, KindleFrontlight};
use plato_core::lightsensor::{LightSensor, KoboLightSensor};
use plato_core::battery::{Battery, KoboBattery, KindleBattery};
use plato_core::geom::{Rectangle, DiagDir, Region};
use plato_core::view::home::Home;
use plato_core::view::reader::Reader;
use plato_core::view::dialog::Dialog;
use plato_core::view::intermission::Intermission;
use plato_core::view::notification::Notification;
use plato_core::device::{CURRENT_DEVICE, Orientation, FrontlightKind};
use plato_core::library::Library;
use plato_core::font::Fonts;
use plato_core::rtc::Rtc;
use plato_core::context::Context;

pub const APP_NAME: &str = "Plato";
const FB_DEVICE: &str = "/dev/fb0";
const RTC_DEVICE: &str = "/dev/rtc0";
// The two halves of the USB mass-storage flow. Named because the share is
// offered only when the enable side is actually installed -- a "yes" that
// cannot be honoured strands the device in the share intermission.
const USB_ENABLE: &str = "scripts/usb-enable.sh";
const USB_DISABLE: &str = "scripts/usb-disable.sh";
// The PW3's touch node is /dev/input/event1 (cyttsp4_mt_b), which the existing
// fallback list already ends in -- Kobo's by-path entries simply do not exist on
// a Kindle, so the loop falls through to it. Verified by reading the list, not
// by probing: PLATO-DEVICE-PROBES confirms the node on device.
const TOUCH_INPUTS: [&str; 5] = ["/dev/input/by-path/platform-2-0010-event",
                                 "/dev/input/by-path/platform-1-0038-event",
                                 "/dev/input/by-path/platform-1-0010-event",
                                 "/dev/input/by-path/platform-0-0010-event",
                                 "/dev/input/event1"];
const BUTTON_INPUTS: [&str; 4] = ["/dev/input/by-path/platform-gpio-keys-event",
                                  "/dev/input/by-path/platform-ntx_event0-event",
                                  "/dev/input/by-path/platform-mxckpd-event",
                                  "/dev/input/event0"];
const POWER_INPUTS: [&str; 3] = ["/dev/input/by-path/platform-bd71828-pwrkey.6.auto-event",
                                 "/dev/input/by-path/platform-bd71828-pwrkey.4.auto-event",
                                 "/dev/input/by-path/platform-bd71828-pwrkey-event"];

const KOBO_UPDATE_BUNDLE: &str = "/mnt/onboard/.kobo/KoboRoot.tgz";

const CLOCK_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const BATTERY_REFRESH_INTERVAL: Duration = Duration::from_secs(299);
const AUTO_SUSPEND_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const SUSPEND_WAIT_DELAY: Duration = Duration::from_secs(15);
const PREPARE_SUSPEND_WAIT_DELAY: Duration = Duration::from_secs(3);

struct Task {
    id: TaskId,
    _chan: Receiver<()>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum TaskId {
    CheckBattery,
    PrepareSuspend,
    Suspend,
}

struct HistoryItem {
    view: Box<dyn View>,
    rotation: i8,
    monochrome: bool,
    dithered: bool,
}

fn build_context(fb: Box<dyn Framebuffer>) -> Result<Context, Error> {
    // The Kindle has /dev/rtc0, but powerd owns it: alarms are set through its
    // `rtcWakeup` lipc property (and only while it is in ReadyToSuspend), and
    // powerd programs the chip through
    // .../max77696-rtc.0/rtc_delta_alarm, not through the RTC ioctls. Writing
    // an alarm here would silently fight powerd's own suspend policy, so the
    // Kindle carries no `Rtc` at all — which also makes every auto-power-off
    // branch in the suspend path a no-op until `rtcWakeup` is wired to it.
    let rtc = if CURRENT_DEVICE.is_kindle() {
        None
    } else {
        Rtc::new(RTC_DEVICE)
            .map_err(|e| eprintln!("Can't open RTC device: {:#}.", e))
            .ok()
    };
    let path = Path::new(SETTINGS_PATH);
    let mut settings = if path.exists() {
        load_toml::<Settings, _>(path).context("can't load settings")?
    } else {
        Default::default()
    };

    if settings.libraries.is_empty() {
        return Err(format_err!("no libraries found"));
    }

    if settings.selected_library >= settings.libraries.len() {
        settings.selected_library = 0;
    }

    let library_settings = &settings.libraries[settings.selected_library];
    let library = Library::new(&library_settings.path, library_settings.mode)?;

    let fonts = Fonts::load().context("can't load fonts")?;

    let battery = if CURRENT_DEVICE.is_kindle() {
        Box::new(KindleBattery::new().context("can't create battery")?) as Box<dyn Battery>
    } else {
        Box::new(KoboBattery::new().context("can't create battery")?) as Box<dyn Battery>
    };

    let lightsensor = if CURRENT_DEVICE.has_lightsensor() {
        Box::new(KoboLightSensor::new().context("can't create light sensor")?) as Box<dyn LightSensor>
    } else {
        Box::new(0u16) as Box<dyn LightSensor>
    };

    let levels = settings.frontlight_levels;
    // The Kindle is checked before frontlight_kind(), which would answer
    // Standard and then reach for Kobo's /dev/ntx_io. LightSensor stays the
    // existing null impl: has_lightsensor() is false for this model.
    let frontlight = if CURRENT_DEVICE.is_kindle() {
        Box::new(KindleFrontlight::new(levels.intensity)
                     .context("can't create frontlight")?) as Box<dyn Frontlight>
    } else {
        match CURRENT_DEVICE.frontlight_kind() {
            FrontlightKind::Standard => Box::new(StandardFrontlight::new(levels.intensity)
                                            .context("can't create standard frontlight")?) as Box<dyn Frontlight>,
            FrontlightKind::Natural => Box::new(NaturalFrontlight::new(levels.intensity, levels.warmth)
                                            .context("can't create natural frontlight")?) as Box<dyn Frontlight>,
            FrontlightKind::Premixed => Box::new(PremixedFrontlight::new(levels.intensity, levels.warmth)
                                            .context("can't create premixed frontlight")?) as Box<dyn Frontlight>,
        }
    };

    Ok(Context::new(fb, rtc, library, settings,
                    fonts, battery, frontlight, lightsensor))
}

fn schedule_task(id: TaskId, event: Event, delay: Duration, hub: &Sender<Event>, tasks: &mut Vec<Task>) {
    let (ty, ry) = mpsc::channel();
    let hub2 = hub.clone();
    tasks.retain(|task| task.id != id);
    tasks.push(Task { id, _chan: ry });
    thread::spawn(move || {
        thread::sleep(delay);
        if ty.send(()).is_ok() {
            hub2.send(event).ok();
        }
    });
}

fn resume(id: TaskId, tasks: &mut Vec<Task>, view: &mut dyn View, hub: &Sender<Event>, rq: &mut RenderQueue, context: &mut Context) {
    if id == TaskId::Suspend {
        tasks.retain(|task| task.id != TaskId::Suspend);
        if context.settings.frontlight {
            let levels = context.settings.frontlight_levels;
            context.frontlight.set_warmth(levels.warmth);
            context.frontlight.set_intensity(levels.intensity);
        }
        if context.settings.wifi {
            // Kindle fork: report the link back, as at startup and in set_wifi.
            if Command::new("scripts/wifi-enable.sh")
                       .status().map(|s| s.success()).unwrap_or(false) {
                hub.send(Event::Device(DeviceEvent::NetUp)).ok();
            }
        }
    }
    if id == TaskId::Suspend || id == TaskId::PrepareSuspend {
        tasks.retain(|task| task.id != TaskId::PrepareSuspend);
        if let Some(index) = locate::<Intermission>(view) {
            let rect = *view.child(index).rect();
            view.children_mut().remove(index);
            rq.add(RenderData::expose(rect, UpdateMode::Full));
        }
        hub.send(Event::ClockTick).ok();
        hub.send(Event::BatteryTick).ok();
    }
}

fn power_off(view: &mut dyn View, history: &mut Vec<HistoryItem>, updating: &mut Vec<UpdateData>, context: &mut Context) {
    let (tx, _rx) = mpsc::channel();
    view.handle_event(&Event::Back, &tx, &mut VecDeque::new(), &mut RenderQueue::new(), context);
    while let Some(mut item) = history.pop() {
        item.view.handle_event(&Event::Back, &tx, &mut VecDeque::new(), &mut RenderQueue::new(), context);
    }
    let interm = Intermission::new(context.fb.rect(), IntermKind::PowerOff, context);
    wait_for_all(updating, context);
    interm.render(context.fb.as_mut(), *interm.rect(), &mut context.fonts);
    context.fb.update(interm.rect(), UpdateMode::Full).ok();
}

// Kindle fork: the enable path reports back.
//
// Upstream relies on a DeviceEvent::NetUp arriving on its own once the link
// comes up. That event is parsed out of /tmp/nickel-hardware-status (Kobo's
// nickel writes it), which does not exist here -- parse_usb_events() opens it,
// fails, and the thread exits, so on the Kindle NetUp is NEVER emitted. Without
// this, enabling WiFi would set context.online = false forever and show no
// notification, i.e. look like nothing happened.
//
// So: run the script, and synthesise NetUp ourselves when it succeeds. The
// script blocks until associated + addressed (2 s typical, 13 s worst measured),
// which is why its exit status is worth waiting for. On failure we roll the
// setting back rather than leave the UI claiming WiFi is on.
fn set_wifi(enable: bool, hub: &Sender<Event>, context: &mut Context) {
    if context.settings.wifi == enable {
        return;
    }
    context.settings.wifi = enable;
    if context.settings.wifi {
        let ok = Command::new("scripts/wifi-enable.sh")
                         .status()
                         .map(|s| s.success())
                         .unwrap_or(false);
        if ok {
            hub.send(Event::Device(DeviceEvent::NetUp)).ok();
        } else {
            context.settings.wifi = false;
            context.online = false;
            hub.send(Event::Notify("Couldn't bring WiFi up.".to_string())).ok();
        }
    } else {
        Command::new("scripts/wifi-disable.sh")
                .status()
                .ok();
        context.online = false;
    }
}

#[derive(PartialEq)]
enum ExitStatus {
    Quit,
    Reboot,
    PowerOff,
}

pub fn run() -> Result<(), Error> {
    let mut inactive_since = Instant::now();
    let mut exit_status = ExitStatus::Quit;

    // The Kindle branch comes first, ahead of the Kobo mark ladder: mark() is
    // 6 for the PW3, which would otherwise select KoboFramebuffer1 and send the
    // lab126 EPDC a 68-byte update struct through the wrong ioctl number.
    let mut fb: Box<dyn Framebuffer> = if CURRENT_DEVICE.is_kindle() {
        Box::new(KindleFramebuffer::new(FB_DEVICE, CURRENT_DEVICE.startup_rotation())
                     .context("can't create framebuffer")?)
    } else if CURRENT_DEVICE.mark() != 8 {
        Box::new(KoboFramebuffer1::new(FB_DEVICE).context("can't create framebuffer")?)
    } else {
        Box::new(KoboFramebuffer2::new(FB_DEVICE).context("can't create framebuffer")?)
    };

    let initial_rotation = CURRENT_DEVICE.transformed_rotation(fb.rotation());
    let startup_rotation = CURRENT_DEVICE.startup_rotation();
    if !CURRENT_DEVICE.has_gyroscope() && initial_rotation != startup_rotation {
        fb.set_rotation(startup_rotation).ok();
    }

    let mut context = build_context(fb).context("can't build context")?;

    context.plugged = context.battery.status().is_ok_and(|v| v[0].is_wired());

    if context.settings.import.startup_trigger {
        context.batch_import();
    }
    context.load_dictionaries();
    context.load_keyboard_layouts();

    let mut paths = Vec::new();
    for ti in &TOUCH_INPUTS {
        if Path::new(ti).exists() {
            paths.push(ti.to_string());
            break;
        }
    }
    for bi in &BUTTON_INPUTS {
        if Path::new(bi).exists() {
            paths.push(bi.to_string());
            break;
        }
    }
    for pi in &POWER_INPUTS {
        if Path::new(pi).exists() {
            paths.push(pi.to_string());
            break;
        }
    }

    let (raw_sender, raw_receiver) = raw_events(paths);
    let touch_screen = gesture_events(device_events(raw_receiver, context.display, context.settings.button_scheme));
    let usb_port = usb_events();

    let (tx, rx) = mpsc::channel();
    let tx2 = tx.clone();

    thread::spawn(move || {
        while let Ok(evt) = touch_screen.recv() {
            tx2.send(evt).ok();
        }
    });

    let tx3 = tx.clone();
    thread::spawn(move || {
        while let Ok(evt) = usb_port.recv() {
            tx3.send(Event::Device(evt)).ok();
        }
    });

    let tx4 = tx.clone();
    thread::spawn(move || {
        loop {
            thread::sleep(CLOCK_REFRESH_INTERVAL);
            tx4.send(Event::ClockTick).ok();
        }
    });

    let tx5 = tx.clone();
    thread::spawn(move || {
        loop {
            thread::sleep(BATTERY_REFRESH_INTERVAL);
            tx5.send(Event::BatteryTick).ok();
        }
    });

    if context.settings.auto_suspend > 0.0 {
        let tx6 = tx.clone();
        thread::spawn(move || {
            loop {
                thread::sleep(AUTO_SUSPEND_REFRESH_INTERVAL);
                tx6.send(Event::MightSuspend).ok();
            }
        });
    }

    context.fb.set_inverted(context.settings.inverted);

    // Kindle fork: same NetUp synthesis as set_wifi -- see its comment. Without
    // it, starting with wifi = true leaves context.online false forever, so the
    // UI never learns it is online even though the link is up.
    if context.settings.wifi {
        if Command::new("scripts/wifi-enable.sh").status().map(|s| s.success()).unwrap_or(false) {
            tx.send(Event::Device(DeviceEvent::NetUp)).ok();
        }
    } else {
        Command::new("scripts/wifi-disable.sh").status().ok();
    }

    if context.settings.frontlight {
        let levels = context.settings.frontlight_levels;
        context.frontlight.set_warmth(levels.warmth);
        context.frontlight.set_intensity(levels.intensity);
    } else {
        context.frontlight.set_intensity(0.0);
        context.frontlight.set_warmth(0.0);
    }

    let mut tasks: Vec<Task> = Vec::new();
    let mut history: Vec<HistoryItem> = Vec::new();
    let mut rq = RenderQueue::new();
    let mut view: Box<dyn View> = Box::new(Home::new(context.fb.rect(), &tx,
                                                     &mut rq, &mut context)?);

    let mut updating = Vec::new();
    let current_dir = env::current_dir()?;

    println!("{} is running on a Kobo {}.", APP_NAME,
                                            CURRENT_DEVICE.model);
    println!("The framebuffer resolution is {} by {}.", context.fb.rect().width(),
                                                        context.fb.rect().height());

    let mut bus = VecDeque::with_capacity(4);

    schedule_task(TaskId::CheckBattery, Event::CheckBattery,
                  BATTERY_REFRESH_INTERVAL, &tx, &mut tasks);
    tx.send(Event::WakeUp).ok();

    while let Ok(evt) = rx.recv() {
        match evt {
            Event::Device(de) => {
                match de {
                    DeviceEvent::Button { code: ButtonCode::Power, status: ButtonStatus::Released, .. } => {
                        if context.shared || context.covered {
                            continue;
                        }

                        if tasks.iter().any(|task| task.id == TaskId::PrepareSuspend) {
                            resume(TaskId::PrepareSuspend, &mut tasks, view.as_mut(), &tx, &mut rq, &mut context);
                        } else if tasks.iter().any(|task| task.id == TaskId::Suspend) {
                            resume(TaskId::Suspend, &mut tasks, view.as_mut(), &tx, &mut rq, &mut context);
                        } else {
                            view.handle_event(&Event::Suspend, &tx, &mut bus, &mut rq, &mut context);
                            let interm = Intermission::new(context.fb.rect(), IntermKind::Suspend, &context);
                            rq.add(RenderData::new(interm.id(), *interm.rect(), UpdateMode::Full));
                            schedule_task(TaskId::PrepareSuspend, Event::PrepareSuspend,
                                          PREPARE_SUSPEND_WAIT_DELAY, &tx, &mut tasks);
                            view.children_mut().push(Box::new(interm) as Box<dyn View>);
                        }
                    },
                    DeviceEvent::Button { code: ButtonCode::Light, status: ButtonStatus::Pressed, .. } => {
                        tx.send(Event::ToggleFrontlight).ok();
                    },
                    DeviceEvent::CoverOn => {
                        if context.covered {
                           continue;
                        }

                        context.covered = true;

                        if !context.settings.sleep_cover || context.shared ||
                           tasks.iter().any(|task| task.id == TaskId::PrepareSuspend ||
                                                   task.id == TaskId::Suspend) {
                            continue;
                        }

                        view.handle_event(&Event::Suspend, &tx, &mut bus, &mut rq, &mut context);
                        let interm = Intermission::new(context.fb.rect(), IntermKind::Suspend, &context);
                        rq.add(RenderData::new(interm.id(), *interm.rect(), UpdateMode::Full));
                        schedule_task(TaskId::PrepareSuspend, Event::PrepareSuspend,
                                      PREPARE_SUSPEND_WAIT_DELAY, &tx, &mut tasks);
                        view.children_mut().push(Box::new(interm) as Box<dyn View>);
                    },
                    DeviceEvent::CoverOff => {
                        if !context.covered {
                           continue;
                        }

                        context.covered = false;

                        if context.shared || !context.settings.sleep_cover {
                            continue;
                        }

                        if tasks.iter().any(|task| task.id == TaskId::PrepareSuspend) {
                            resume(TaskId::PrepareSuspend, &mut tasks, view.as_mut(), &tx, &mut rq, &mut context);
                        } else if tasks.iter().any(|task| task.id == TaskId::Suspend) {
                            resume(TaskId::Suspend, &mut tasks, view.as_mut(), &tx, &mut rq, &mut context);
                        }
                    },
                    DeviceEvent::NetUp => {
                        if tasks.iter().any(|task| task.id == TaskId::PrepareSuspend ||
                                                   task.id == TaskId::Suspend) {
                            continue;
                        }
                        let ip = Command::new("scripts/ip.sh").output()
                                         .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
                                         .unwrap_or_default();
                        let essid = Command::new("scripts/essid.sh").output()
                                            .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
                                            .unwrap_or_default();
                        // Kindle fork: signal strength, from wpa_cli signal_poll.
                        // Empty when not associated, so an empty string means
                        // "no reading" rather than an error to render.
                        let rssi = Command::new("scripts/signal.sh").output()
                                           .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
                                           .unwrap_or_default();
                        let msg = if rssi.is_empty() {
                            format!("Network is up ({}, {}).", ip, essid)
                        } else {
                            format!("Network is up ({}, {}, {} dBm).", ip, essid, rssi)
                        };
                        let notif = Notification::new(msg, &tx, &mut rq, &mut context);
                        context.online = true;
                        view.children_mut().push(Box::new(notif) as Box<dyn View>);
                        if view.is::<Home>() {
                            view.handle_event(&evt, &tx, &mut bus, &mut rq, &mut context);
                        } else if let Some(entry) = history.get_mut(0).filter(|entry| entry.view.is::<Home>()) {
                            let (tx, _rx) = mpsc::channel();
                            entry.view.handle_event(&evt, &tx, &mut VecDeque::new(), &mut RenderQueue::new(), &mut context);
                        }
                    },
                    DeviceEvent::Plug(power_source) => {
                        if context.plugged {
                            continue;
                        }

                        context.plugged = true;

                        tasks.retain(|task| task.id != TaskId::CheckBattery);

                        if context.covered {
                            continue;
                        }

                        match power_source {
                            PowerSource::Wall => {
                                if tasks.iter().any(|task| task.id == TaskId::Suspend) {
                                    continue;
                                }
                            },
                            PowerSource::Host => {
                                if tasks.iter().any(|task| task.id == TaskId::PrepareSuspend) {
                                    resume(TaskId::PrepareSuspend, &mut tasks, view.as_mut(), &tx, &mut rq, &mut context);
                                } else if tasks.iter().any(|task| task.id == TaskId::Suspend) {
                                    resume(TaskId::Suspend, &mut tasks, view.as_mut(), &tx, &mut rq, &mut context);
                                }

                                // Only offer to share storage if the script
                                // that does it exists. Without it the answer
                                // "yes" leads to the share intermission with
                                // nothing shared and no way out but unplugging
                                // the cable -- the same trap as a menu entry
                                // for a program that is not installed.
                                if !is_installed(USB_ENABLE) {
                                    eprintln!("Not offering to share storage: {} is missing.", USB_ENABLE);
                                } else if context.settings.auto_share {
                                    tx.send(Event::PrepareShare).ok();
                                } else {
                                    let dialog = Dialog::new(ViewId::ShareDialog,
                                                             Some(Event::PrepareShare),
                                                             "Share storage via USB?".to_string(),
                                                             &mut context);
                                    rq.add(RenderData::new(dialog.id(), *dialog.rect(), UpdateMode::Gui));
                                    view.children_mut().push(Box::new(dialog) as Box<dyn View>);
                                }

                                inactive_since = Instant::now();
                            },
                        }

                        tx.send(Event::BatteryTick).ok();
                    },
                    DeviceEvent::Unplug(..) => {
                        if !context.plugged {
                            continue;
                        }

                        if context.shared {
                            context.shared = false;
                            Command::new(USB_DISABLE).status()
                                    .map_err(|e| eprintln!("Can't run {}: {:#}.", USB_DISABLE, e))
                                    .ok();
                            env::set_current_dir(&current_dir)
                                .map_err(|e| eprintln!("Can't set current directory to {}: {:#}.", current_dir.display(), e))
                                .ok();
                            let path = Path::new(SETTINGS_PATH);
                            if let Ok(settings) = load_toml::<Settings, _>(path)
                                                            .map_err(|e| eprintln!("Can't load settings: {:#}.", e)) {
                                context.settings = settings;
                            }
                            if context.settings.wifi {
                                Command::new("scripts/wifi-enable.sh")
                                        .status()
                                        .ok();
                            }
                            if context.settings.frontlight {
                                let levels = context.settings.frontlight_levels;
                                context.frontlight.set_warmth(levels.warmth);
                                context.frontlight.set_intensity(levels.intensity);
                            }
                            if let Some(index) = locate::<Intermission>(view.as_ref()) {
                                let rect = *view.child(index).rect();
                                view.children_mut().remove(index);
                                rq.add(RenderData::expose(rect, UpdateMode::Full));
                            }
                            if Path::new(KOBO_UPDATE_BUNDLE).exists() {
                                tx.send(Event::Select(EntryId::Reboot)).ok();
                            }
                            context.library.reload();
                            if context.settings.import.unshare_trigger {
                                context.batch_import();
                            }
                            view.handle_event(&Event::Reseed, &tx, &mut bus, &mut rq, &mut context);
                        } else {
                            context.plugged = false;
                            schedule_task(TaskId::CheckBattery, Event::CheckBattery,
                                          BATTERY_REFRESH_INTERVAL, &tx, &mut tasks);
                            if tasks.iter().any(|task| task.id == TaskId::Suspend) {
                                if !context.covered {
                                    resume(TaskId::Suspend, &mut tasks, view.as_mut(), &tx, &mut rq, &mut context);
                                }
                            } else {
                                tx.send(Event::BatteryTick).ok();
                            }
                        }
                    },
                    DeviceEvent::RotateScreen(n) => {
                        if context.shared || tasks.iter().any(|task| task.id == TaskId::PrepareSuspend ||
                                                                     task.id == TaskId::Suspend) {
                            continue;
                        }

                        if view.is::<RotationValues>() {
                            println!("Gyro rotation: {}", n);
                        }

                        if let Some(rotation_lock) = context.settings.rotation_lock {
                            let orientation = CURRENT_DEVICE.orientation(n);
                            if rotation_lock == RotationLock::Current ||
                               (rotation_lock == RotationLock::Portrait && orientation == Orientation::Landscape) ||
                               (rotation_lock == RotationLock::Landscape && orientation == Orientation::Portrait) {
                                continue;
                            }
                        }

                        tx.send(Event::Select(EntryId::Rotate(n))).ok();
                    },
                    DeviceEvent::UserActivity if context.settings.auto_suspend > 0.0 => {
                        inactive_since = Instant::now();
                    },
                    _ => {
                        handle_event(view.as_mut(), &evt, &tx, &mut bus, &mut rq, &mut context);
                    }
                }
            },
            Event::CheckBattery => {
                schedule_task(TaskId::CheckBattery, Event::CheckBattery,
                              BATTERY_REFRESH_INTERVAL, &tx, &mut tasks);
                if tasks.iter().any(|task| task.id == TaskId::PrepareSuspend ||
                                           task.id == TaskId::Suspend) {
                    continue;
                }
                if let Ok(v) = context.battery.capacity().map(|v| v[0]) {
                    if v < context.settings.battery.power_off {
                        power_off(view.as_mut(), &mut history, &mut updating, &mut context);
                        exit_status = ExitStatus::PowerOff;
                        break;
                    } else if v < context.settings.battery.warn {
                        let notif = Notification::new("The battery capacity is getting low.".to_string(),
                                                      &tx, &mut rq, &mut context);
                        view.children_mut().push(Box::new(notif) as Box<dyn View>);
                    }
                }
            },
            Event::PrepareSuspend => {
                tasks.retain(|task| task.id != TaskId::PrepareSuspend);
                wait_for_all(&mut updating, &mut context);
                let path = Path::new(SETTINGS_PATH);
                save_toml(&context.settings, path).map_err(|e| eprintln!("Can't save settings: {:#}.", e)).ok();
                context.library.flush();

                if context.settings.frontlight {
                    context.settings.frontlight_levels = context.frontlight.levels();
                    context.frontlight.set_intensity(0.0);
                    context.frontlight.set_warmth(0.0);
                }
                if context.settings.wifi {
                    Command::new("scripts/wifi-disable.sh")
                            .status()
                            .ok();
                    context.online = false;
                }
                // https://github.com/koreader/koreader/commit/71afe36
                schedule_task(TaskId::Suspend, Event::Suspend,
                              SUSPEND_WAIT_DELAY, &tx, &mut tasks);
            },
            Event::Suspend => {
                if context.settings.auto_power_off > 0.0 {
                    context.rtc.iter().for_each(|rtc| {
                        rtc.set_alarm(context.settings.auto_power_off)
                           .map_err(|e| eprintln!("Can't set alarm: {:#}.", e))
                           .ok();
                    });
                }
                let before = Local::now();
                println!("{}", before.format("Went to sleep on %B %-d, %Y at %H:%M:%S."));
                let status = Command::new("scripts/suspend.sh").status();
                let outcome = suspend_outcome(&status);
                if outcome != SuspendOutcome::Slept {
                    // We are still awake. Upstream re-schedules the suspend
                    // task below unconditionally, relying on the wake event to
                    // cancel it — with no sleep there is no wake, so that
                    // becomes a retry every SUSPEND_WAIT_DELAY behind a
                    // Sleeping screen that never lifts. Go back to being awake
                    // instead, and say why: on this device the helper declines
                    // whenever the daemon that owns suspend will not act.
                    let msg = if outcome == SuspendOutcome::Refused {
                        "Suspend was refused; staying awake."
                    } else {
                        "Can't run the suspend script."
                    };
                    println!("{}", before.format("Didn't sleep on %B %-d, %Y at %H:%M:%S."));
                    if context.settings.auto_power_off > 0.0 {
                        context.rtc.iter().for_each(|rtc| {
                            rtc.disable_alarm()
                               .map_err(|e| eprintln!("Can't disable alarm: {:#}.", e))
                               .ok();
                        });
                    }
                    resume(TaskId::Suspend, &mut tasks, view.as_mut(), &tx, &mut rq, &mut context);
                    let notif = Notification::new(msg.to_string(), &tx, &mut rq, &mut context);
                    view.children_mut().push(Box::new(notif) as Box<dyn View>);
                    inactive_since = Instant::now();
                    continue;
                }
                let after = Local::now();
                println!("{}", after.format("Woke up on %B %-d, %Y at %H:%M:%S."));
                Command::new("scripts/resume.sh")
                        .status()
                        .ok();
                inactive_since = Instant::now();
                // If the wake is legitimate, the task will be cancelled by `resume`.
                schedule_task(TaskId::Suspend, Event::Suspend,
                              SUSPEND_WAIT_DELAY, &tx, &mut tasks);
                if context.settings.auto_power_off > 0.0 {
                    let dur = plato_core::chrono::Duration::seconds((86_400.0 * context.settings.auto_power_off) as i64);
                    if let Some(fired) = context.rtc.as_ref()
                                                .and_then(|rtc| rtc.alarm()
                                                                   .map_err(|e| eprintln!("Can't get alarm: {:#}", e))
                                                                   .map(|rwa| !rwa.enabled() ||
                                                                              (rwa.year() <= 1970 &&
                                                                               ((after - before) - dur).num_seconds().abs() < 3))
                                                                   .ok()) {
                        if fired {
                            power_off(view.as_mut(), &mut history, &mut updating, &mut context);
                            exit_status = ExitStatus::PowerOff;
                            break;
                        } else {
                            context.rtc.iter().for_each(|rtc| {
                                rtc.disable_alarm()
                                   .map_err(|e| eprintln!("Can't disable alarm: {:#}.", e))
                                   .ok();
                            });
                        }
                    }
                }
            },
            Event::PrepareShare => {
                if context.shared {
                    continue;
                }

                tasks.clear();
                view.handle_event(&Event::Back, &tx, &mut bus, &mut rq, &mut context);
                while let Some(mut item) = history.pop() {
                    item.view.handle_event(&Event::Back, &tx, &mut bus, &mut rq, &mut context);
                    if item.rotation != context.display.rotation {
                        wait_for_all(&mut updating, &mut context);
                        if let Ok(dims) = context.fb.set_rotation(item.rotation) {
                            raw_sender.send(display_rotate_event(item.rotation)).ok();
                            context.display.rotation = item.rotation;
                            context.display.dims = dims;
                        }
                    }
                    view = item.view;
                }
                let path = Path::new(SETTINGS_PATH);
                save_toml(&context.settings, path)
                         .map_err(|e| eprintln!("Can't save settings: {:#}.", e)).ok();
                context.library.flush();

                if context.settings.frontlight {
                    context.settings.frontlight_levels = context.frontlight.levels();
                    context.frontlight.set_intensity(0.0);
                    context.frontlight.set_warmth(0.0);
                }
                if context.settings.wifi {
                    Command::new("scripts/wifi-disable.sh")
                            .status()
                            .ok();
                    context.online = false;
                }

                let interm = Intermission::new(context.fb.rect(), IntermKind::Share, &context);
                rq.add(RenderData::new(interm.id(), *interm.rect(), UpdateMode::Full));
                view.children_mut().push(Box::new(interm) as Box<dyn View>);
                tx.send(Event::Share).ok();
            },
            Event::Share => {
                if context.shared {
                    continue;
                }

                match Command::new(USB_ENABLE).status() {
                    Ok(_) => context.shared = true,
                    Err(e) => {
                        // Say so rather than sitting on the intermission
                        // screen pretending the volume is exported.
                        eprintln!("Can't run {}: {:#}.", USB_ENABLE, e);
                        tx.send(Event::Notify("Can't share storage.".to_string())).ok();
                    },
                }
            },
            Event::Gesture(ge) => {
                match ge {
                    GestureEvent::HoldButtonLong(ButtonCode::Power) => {
                        power_off(view.as_mut(), &mut history, &mut updating, &mut context);
                        exit_status = ExitStatus::PowerOff;
                        break;
                    },
                    GestureEvent::MultiTap(mut points) => {
                        if points[0].x > points[1].x {
                            points.swap(0, 1);
                        }
                        let rect = context.fb.rect();
                        let r1 = Region::from_point(points[0], rect,
                                                    context.settings.reader.strip_width,
                                                    context.settings.reader.corner_width);
                        let r2 = Region::from_point(points[1], rect,
                                                    context.settings.reader.strip_width,
                                                    context.settings.reader.corner_width);
                        match (r1, r2) {
                            (Region::Corner(DiagDir::SouthWest), Region::Corner(DiagDir::NorthEast)) => {
                                rq.add(RenderData::new(view.id(), context.fb.rect(), UpdateMode::Full));
                            },
                            (Region::Corner(DiagDir::NorthWest), Region::Corner(DiagDir::SouthEast)) => {
                                tx.send(Event::Select(EntryId::TakeScreenshot)).ok();
                            },
                            _ => (),
                        }
                    },
                    _ => {
                        handle_event(view.as_mut(), &evt, &tx, &mut bus, &mut rq, &mut context);
                    },
                }
            },
            Event::ToggleFrontlight => {
                context.set_frontlight(!context.settings.frontlight);
                view.handle_event(&Event::ToggleFrontlight, &tx, &mut bus, &mut rq, &mut context);
            },
            Event::Open(info) => {
                let rotation = context.display.rotation;
                let dithered = context.fb.dithered();
                if let Some(reader_info) = info.reader.as_ref() {
                    if let Some(n) = reader_info.rotation.map(|n| CURRENT_DEVICE.from_canonical(n)) {
                        if CURRENT_DEVICE.orientation(n) != CURRENT_DEVICE.orientation(rotation) {
                            wait_for_all(&mut updating, &mut context);
                            if let Ok(dims) = context.fb.set_rotation(n) {
                                raw_sender.send(display_rotate_event(n)).ok();
                                context.display.rotation = n;
                                context.display.dims = dims;
                            }
                        }
                    }
                    context.fb.set_dithered(reader_info.dithered);
                } else {
                    context.fb.set_dithered(context.settings.reader.dithered_kinds.contains(&info.file.kind));
                }
                let path = info.file.path.clone();
                if let Some(r) = Reader::new(context.fb.rect(), *info, &tx, &mut context) {
                    let mut next_view = Box::new(r) as Box<dyn View>;
                    transfer_notifications(view.as_mut(), next_view.as_mut(), &mut rq, &mut context);
                    history.push(HistoryItem {
                        view,
                        rotation,
                        monochrome: context.fb.monochrome(),
                        dithered,
                    });
                    view = next_view;
                } else {
                    if context.display.rotation != rotation {
                        if let Ok(dims) = context.fb.set_rotation(rotation) {
                            raw_sender.send(display_rotate_event(rotation)).ok();
                            context.display.rotation = rotation;
                            context.display.dims = dims;
                        }
                    }
                    context.fb.set_dithered(dithered);
                    handle_event(view.as_mut(), &Event::Invalid(path), &tx, &mut bus, &mut rq, &mut context);
                }
            },
            Event::Select(EntryId::About) => {
                let dialog = Dialog::new(ViewId::AboutDialog,
                                         None,
                                         format!("Plato {}", env!("CARGO_PKG_VERSION")),
                                         &mut context);
                rq.add(RenderData::new(dialog.id(), *dialog.rect(), UpdateMode::Gui));
                view.children_mut().push(Box::new(dialog) as Box<dyn View>);
            },
            Event::Select(EntryId::SystemInfo) => {
                view.children_mut().retain(|child| !child.is::<Menu>());
                let html = sys_info_as_html();
                let r = Reader::from_html(context.fb.rect(), &html, None, &tx, &mut context);
                let mut next_view = Box::new(r) as Box<dyn View>;
                transfer_notifications(view.as_mut(), next_view.as_mut(), &mut rq, &mut context);
                history.push(HistoryItem {
                    view,
                    rotation: context.display.rotation,
                    monochrome: context.fb.monochrome(),
                    dithered: context.fb.dithered(),
                });
                view = next_view;
            },
            Event::OpenHtml(ref html, ref link_uri) => {
                view.children_mut().retain(|child| !child.is::<Menu>());
                let r = Reader::from_html(context.fb.rect(), html, link_uri.as_deref(), &tx, &mut context);
                let mut next_view = Box::new(r) as Box<dyn View>;
                transfer_notifications(view.as_mut(), next_view.as_mut(), &mut rq, &mut context);
                history.push(HistoryItem {
                    view,
                    rotation: context.display.rotation,
                    monochrome: context.fb.monochrome(),
                    dithered: context.fb.dithered(),
                });
                view = next_view;
            },
            Event::Select(EntryId::Launch(app_cmd)) => {
                view.children_mut().retain(|child| !child.is::<Menu>());
                let monochrome = context.fb.monochrome();
                // An app that fails to start is a notification, never an exit.
                // `Calculator::new` spawns `ivy`, and this arm used to end in
                // `?`: with `ivy` missing from the payload, opening the
                // calculator took the whole process down with "No such file or
                // directory" -- on a device where Plato is the only thing
                // running and there is nothing to restart it with. The menu no
                // longer offers an app whose helper is absent; this is the
                // backstop for whatever gets past that.
                let next_view: Option<Box<dyn View>> = match app_cmd {
                    AppCmd::Sketch => {
                        context.fb.set_monochrome(true);
                        Some(Box::new(Sketch::new(context.fb.rect(), &mut rq, &mut context)) as Box<dyn View>)
                    },
                    AppCmd::Calculator => {
                        match Calculator::new(context.fb.rect(), &tx, &mut rq, &mut context) {
                            Ok(calculator) => Some(Box::new(calculator) as Box<dyn View>),
                            Err(e) => {
                                eprintln!("Can't launch the calculator: {:#}.", e);
                                tx.send(Event::Notify("Can't launch the calculator.".to_string())).ok();
                                None
                            },
                        }
                    },
                    AppCmd::Dictionary { ref query, ref language } => Some(Box::new(DictionaryApp::new(context.fb.rect(), query,
                                                                                                       language, &tx, &mut rq, &mut context)) as Box<dyn View>),
                    AppCmd::TouchEvents => {
                        Some(Box::new(TouchEvents::new(context.fb.rect(), &mut rq, &mut context)) as Box<dyn View>)
                    },
                    AppCmd::RotationValues => {
                        Some(Box::new(RotationValues::new(context.fb.rect(), &mut rq, &mut context)) as Box<dyn View>)
                    },
                };

                if let Some(mut next_view) = next_view {
                    transfer_notifications(view.as_mut(), next_view.as_mut(), &mut rq, &mut context);
                    history.push(HistoryItem {
                        view,
                        rotation: context.display.rotation,
                        monochrome,
                        dithered: context.fb.dithered(),
                    });
                    view = next_view;
                } else if context.fb.monochrome() != monochrome {
                    // Nothing was pushed, so nothing will pop and restore it.
                    context.fb.set_monochrome(monochrome);
                }
            },
            Event::Back => {
                if let Some(item) = history.pop() {
                    view = item.view;
                    if item.monochrome != context.fb.monochrome() {
                        context.fb.set_monochrome(item.monochrome);
                    }
                    if item.dithered != context.fb.dithered() {
                        context.fb.set_dithered(item.dithered);
                    }
                    if CURRENT_DEVICE.orientation(item.rotation) != CURRENT_DEVICE.orientation(context.display.rotation) {
                        wait_for_all(&mut updating, &mut context);
                        if let Ok(dims) = context.fb.set_rotation(item.rotation) {
                            raw_sender.send(display_rotate_event(item.rotation)).ok();
                            context.display.rotation = item.rotation;
                            context.display.dims = dims;
                        }
                    }
                    view.handle_event(&Event::Reseed, &tx, &mut bus, &mut rq, &mut context);
                } else if !view.is::<Home>() {
                    break;
                }
            },
            Event::TogglePresetMenu(rect, index) => {
                if let Some(index) = locate_by_id(view.as_ref(), ViewId::PresetMenu) {
                    let rect = *view.child(index).rect();
                    view.children_mut().remove(index);
                    rq.add(RenderData::expose(rect, UpdateMode::Gui));
                } else {
                    let preset_menu = Menu::new(rect, ViewId::PresetMenu, MenuKind::Contextual,
                                                vec![EntryKind::Command("Remove".to_string(),
                                                                        EntryId::RemovePreset(index))],
                                                &mut context);
                    rq.add(RenderData::new(preset_menu.id(), *preset_menu.rect(), UpdateMode::Gui));
                    view.children_mut().push(Box::new(preset_menu) as Box<dyn View>);
                }
            },
            Event::Show(ViewId::Frontlight) => {
                if !context.settings.frontlight {
                    context.set_frontlight(true);
                    view.handle_event(&Event::ToggleFrontlight, &tx, &mut bus, &mut rq, &mut context);
                }
                let flw = FrontlightWindow::new(&mut context);
                rq.add(RenderData::new(flw.id(), *flw.rect(), UpdateMode::Gui));
                view.children_mut().push(Box::new(flw) as Box<dyn View>);
            },
            Event::ToggleInputHistoryMenu(id, rect) => {
                toggle_input_history_menu(view.as_mut(), id, rect, None, &mut rq, &mut context);
            },
            Event::ToggleNear(ViewId::KeyboardLayoutMenu, rect) => {
                toggle_keyboard_layout_menu(view.as_mut(), rect, None, &mut rq, &mut context);
            },
            Event::Close(ViewId::Frontlight) => {
                if let Some(index) = locate::<FrontlightWindow>(view.as_ref()) {
                    let rect = *view.child(index).rect();
                    view.children_mut().remove(index);
                    rq.add(RenderData::expose(rect, UpdateMode::Gui));
                }
            },
            Event::Close(id) => {
                if let Some(index) = locate_by_id(view.as_ref(), id) {
                    let rect = overlapping_rectangle(view.child(index));
                    rq.add(RenderData::expose(rect, UpdateMode::Gui));
                    view.children_mut().remove(index);
                }
            },
            Event::Select(EntryId::ToggleInverted) => {
                context.fb.toggle_inverted();
                context.settings.inverted = context.fb.inverted();
                rq.add(RenderData::new(view.id(), context.fb.rect(), UpdateMode::Full));
            },
            Event::Select(EntryId::ToggleDithered) => {
                context.fb.toggle_dithered();
                rq.add(RenderData::new(view.id(), context.fb.rect(), UpdateMode::Full));
            },
            Event::Select(EntryId::Rotate(n)) if n != context.display.rotation && view.might_rotate() => {
                wait_for_all(&mut updating, &mut context);
                if let Ok(dims) = context.fb.set_rotation(n) {
                    raw_sender.send(display_rotate_event(n)).ok();
                    context.display.rotation = n;
                    let fb_rect = Rectangle::from(dims);
                    if context.display.dims != dims {
                        context.display.dims = dims;
                        view.resize(fb_rect, &tx, &mut rq, &mut context);
                    } else {
                        rq.add(RenderData::new(view.id(), context.fb.rect(), UpdateMode::Full));
                    }
                }
            },
            Event::Select(EntryId::SetRotationLock(rotation_lock)) => {
                context.settings.rotation_lock = rotation_lock;

            },
            Event::Select(EntryId::SetButtonScheme(button_scheme)) => {
                context.settings.button_scheme = button_scheme;

                // Sending a pseudo event into the raw_events channel toggles the inversion in the device_events channel
                match button_scheme {
                    ButtonScheme::Natural => {
                        raw_sender.send(button_scheme_event(VAL_RELEASE)).ok();
                    },
                    ButtonScheme::Inverted => {
                        raw_sender.send(button_scheme_event(VAL_PRESS)).ok();
                    }
                }
            },
            Event::SetWifi(enable) => {
                set_wifi(enable, &tx, &mut context);
            },
            Event::Select(EntryId::ToggleWifi) => {
                set_wifi(!context.settings.wifi, &tx, &mut context);
            },
            Event::Select(EntryId::TakeScreenshot) => {
                let name = Local::now().format("screenshot-%Y%m%d_%H%M%S.png");
                let msg = match context.fb.save(&name.to_string()) {
                    Err(e) => format!("{}", e),
                    Ok(_) => format!("Saved {}.", name),
                };
                let notif = Notification::new(msg, &tx, &mut rq, &mut context);
                view.children_mut().push(Box::new(notif) as Box<dyn View>);
            },
            Event::CheckFetcher(..) |
            Event::FetcherAddDocument(..) |
            Event::FetcherRemoveDocument(..) |
            Event::FetcherSearch { .. } if !view.is::<Home>() => {
                if let Some(entry) = history.get_mut(0).filter(|entry| entry.view.is::<Home>()) {
                    let (tx, _rx) = mpsc::channel();
                    entry.view.handle_event(&evt, &tx, &mut VecDeque::new(), &mut RenderQueue::new(), &mut context);
                }
            },
            Event::Notify(msg) => {
                let notif = Notification::new(msg, &tx, &mut rq, &mut context);
                view.children_mut().push(Box::new(notif) as Box<dyn View>);
            },
            Event::Select(EntryId::Reboot) => {
                exit_status = ExitStatus::Reboot;
                break;
            },
            Event::Select(EntryId::Quit) => {
                break;
            },
            Event::MightSuspend if context.settings.auto_suspend > 0.0 => {
                if context.shared || tasks.iter().any(|task| task.id == TaskId::PrepareSuspend ||
                                                             task.id == TaskId::Suspend) {
                    inactive_since = Instant::now();
                    continue;
                }
                let seconds = 60.0 * context.settings.auto_suspend;
                if inactive_since.elapsed() > Duration::from_secs_f32(seconds) {
                    view.handle_event(&Event::Suspend, &tx, &mut bus, &mut rq, &mut context);
                    let interm = Intermission::new(context.fb.rect(), IntermKind::Suspend, &context);
                    rq.add(RenderData::new(interm.id(), *interm.rect(), UpdateMode::Full));
                    schedule_task(TaskId::PrepareSuspend, Event::PrepareSuspend,
                                  PREPARE_SUSPEND_WAIT_DELAY, &tx, &mut tasks);
                    view.children_mut().push(Box::new(interm) as Box<dyn View>);
                }
            },
            _ => {
                handle_event(view.as_mut(), &evt, &tx, &mut bus, &mut rq, &mut context);
            },
        }

        process_render_queue(view.as_ref(), &mut rq, &mut context, &mut updating);

        while let Some(ce) = bus.pop_front() {
            tx.send(ce).ok();
        }
    }

    if exit_status == ExitStatus::Quit && !CURRENT_DEVICE.has_gyroscope() && context.display.rotation != initial_rotation {
        context.fb.set_rotation(initial_rotation).ok();
    }

    if tasks.iter().all(|task| task.id != TaskId::Suspend) {
        if context.settings.frontlight {
            context.settings.frontlight_levels = context.frontlight.levels();
        }
    }

    context.library.flush();

    let path = Path::new(SETTINGS_PATH);
    save_toml(&context.settings, path).context("can't save settings")?;

    match exit_status {
        ExitStatus::Reboot => {
            File::create("/tmp/reboot").ok();
        },
        ExitStatus::PowerOff => {
            File::create("/tmp/power_off").ok();
        },
        _ => (),
    }

    Ok(())
}
