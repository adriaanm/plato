use std::sync::mpsc;
use chrono::Local;
use crate::device::CURRENT_DEVICE;
use crate::settings::{ButtonScheme, RotationLock};
use crate::framebuffer::UpdateMode;
use crate::geom::{Point, Rectangle};
use super::{View, RenderQueue, RenderData, ViewId, AppCmd, EntryId, EntryKind};
use super::menu::{Menu, MenuKind};
use super::notification::Notification;
use crate::context::Context;

pub fn shift(view: &mut dyn View, delta: Point) {
    *view.rect_mut() += delta;
    for child in view.children_mut().iter_mut() {
        shift(child.as_mut(), delta);
    }
}

pub fn locate<T: View>(view: &dyn View) -> Option<usize> {
    for (index, child) in view.children().iter().enumerate() {
        if child.as_ref().is::<T>() {
            return Some(index);
        }
    }
    None
}

pub fn rlocate<T: View>(view: &dyn View) -> Option<usize> {
    for (index, child) in view.children().iter().enumerate().rev() {
        if child.as_ref().is::<T>() {
            return Some(index);
        }
    }
    None
}

pub fn locate_by_id(view: &dyn View, id: ViewId) -> Option<usize> {
    view.children().iter().position(|c| c.view_id().map_or(false, |i| i == id))
}

pub fn overlapping_rectangle(view: &dyn View) -> Rectangle {
    let mut rect = *view.rect();
    for child in view.children() {
        rect.absorb(&overlapping_rectangle(child.as_ref()));
    }
    rect
}

// Transfer the notifications from the view1 to the view2.
pub fn transfer_notifications(view1: &mut dyn View, view2: &mut dyn View, rq: &mut RenderQueue, context: &mut Context) {
    for index in (0..view1.len()).rev() {
        if view1.child(index).is::<Notification>() {
            let mut child = view1.children_mut().remove(index);
            if view2.rect() != view1.rect() {
                let (tx, _rx) = mpsc::channel();
                child.resize(*view2.rect(), &tx, rq, context);
            }
            view2.children_mut().push(child);
        }
    }
}

/// The Applications submenu, minus every app whose external helper is not
/// installed.
///
/// Offering an app that cannot start is worse than not offering it: launching
/// the calculator on a device with no `ivy` in the payload used to take the
/// whole process down, and on a device with no shell there is nothing to
/// restart it with. The spawn is now non-fatal too, but that is the backstop —
/// this is the fix.
///
/// The predicate is a parameter so the filtering can be tested without a
/// filesystem; production passes [`AppCmd::is_available`].
pub fn application_entries(is_available: impl Fn(&AppCmd) -> bool) -> Vec<EntryKind> {
    let apps = [("Dictionary", AppCmd::Dictionary { query: String::new(), language: String::new() }),
                ("Calculator", AppCmd::Calculator),
                ("Sketch", AppCmd::Sketch)];
    let tools = [("Touch Events", AppCmd::TouchEvents),
                 ("Rotation Values", AppCmd::RotationValues)];

    let entry = |(label, cmd): &(&str, AppCmd)| {
        EntryKind::Command(label.to_string(), EntryId::Launch(cmd.clone()))
    };

    let mut entries: Vec<EntryKind> = apps.iter().filter(|(_, cmd)| is_available(cmd))
                                          .map(entry).collect();
    entries.push(EntryKind::Separator);
    entries.extend(tools.iter().filter(|(_, cmd)| is_available(cmd)).map(entry));
    tidy_separators(entries)
}

/// Drop separators that no longer separate anything: leading, trailing, or
/// doubled.
///
/// Filtering a menu leaves them behind, and a menu that opens on a horizontal
/// rule looks like a bug in the menu rather than a missing program.
fn tidy_separators(entries: Vec<EntryKind>) -> Vec<EntryKind> {
    let mut tidy: Vec<EntryKind> = Vec::with_capacity(entries.len());

    for entry in entries {
        let is_separator = matches!(entry, EntryKind::Separator);
        if is_separator && matches!(tidy.last(), None | Some(EntryKind::Separator)) {
            continue;
        }
        tidy.push(entry);
    }

    if matches!(tidy.last(), Some(EntryKind::Separator)) {
        tidy.pop();
    }

    tidy
}

pub fn toggle_main_menu(view: &mut dyn View, rect: Rectangle, enable: Option<bool>, rq: &mut RenderQueue, context: &mut Context) {
    if let Some(index) = locate_by_id(view, ViewId::MainMenu) {
        if let Some(true) = enable {
            return;
        }
        rq.add(RenderData::expose(*view.child(index).rect(), UpdateMode::Gui));
        view.children_mut().remove(index);
    } else {
        if let Some(false) = enable {
            return;
        }

        let rotation = CURRENT_DEVICE.to_canonical(context.display.rotation);
        let rotate = (0..4).map(|n|
            EntryKind::RadioButton((n as i16 * 90).to_string(),
                                   EntryId::Rotate(CURRENT_DEVICE.from_canonical(n)),
                                   n == rotation)
        ).collect::<Vec<EntryKind>>();

        let apps = application_entries(AppCmd::is_available);
        let mut entries = vec![EntryKind::Command("About".to_string(),
                                                  EntryId::About),
                               EntryKind::Command("System Info".to_string(),
                                                  EntryId::SystemInfo),
                               EntryKind::Separator,
                               EntryKind::CheckBox("Invert Colors".to_string(),
                                                   EntryId::ToggleInverted,
                                                   context.fb.inverted()),
                               EntryKind::CheckBox("Enable WiFi".to_string(),
                                                   EntryId::ToggleWifi,
                                                   context.settings.wifi),
                               EntryKind::Separator,
                               EntryKind::SubMenu("Rotate".to_string(), rotate),
                               EntryKind::Command("Take Screenshot".to_string(),
                                                  EntryId::TakeScreenshot),
                               EntryKind::Separator];

        // An Applications submenu with nothing runnable in it would be a
        // submenu that opens on an empty list.
        if !apps.is_empty() {
            entries.push(EntryKind::SubMenu("Applications".to_string(), apps));
            entries.push(EntryKind::Separator);
        }

        entries.push(EntryKind::Command("Reboot".to_string(), EntryId::Reboot));
        entries.push(EntryKind::Command("Quit".to_string(), EntryId::Quit));

        if CURRENT_DEVICE.has_page_turn_buttons() {
            let button_scheme = context.settings.button_scheme;
            let button_schemes = vec![
                EntryKind::RadioButton(ButtonScheme::Natural.to_string(), EntryId::SetButtonScheme(ButtonScheme::Natural), button_scheme == ButtonScheme::Natural),
                EntryKind::RadioButton(ButtonScheme::Inverted.to_string(), EntryId::SetButtonScheme(ButtonScheme::Inverted), button_scheme == ButtonScheme::Inverted),
            ];
            entries.insert(5, EntryKind::SubMenu("Button Scheme".to_string(), button_schemes));
        }

        if CURRENT_DEVICE.has_gyroscope() {
            let rotation_lock = context.settings.rotation_lock;
            let gyro = vec![
                EntryKind::RadioButton("Auto".to_string(), EntryId::SetRotationLock(None), rotation_lock.is_none()),
                EntryKind::Separator,
                EntryKind::RadioButton("Portrait".to_string(), EntryId::SetRotationLock(Some(RotationLock::Portrait)), rotation_lock == Some(RotationLock::Portrait)),
                EntryKind::RadioButton("Landscape".to_string(), EntryId::SetRotationLock(Some(RotationLock::Landscape)), rotation_lock == Some(RotationLock::Landscape)),
                EntryKind::RadioButton("Ignore".to_string(), EntryId::SetRotationLock(Some(RotationLock::Current)), rotation_lock == Some(RotationLock::Current)),
            ];
            entries.insert(5, EntryKind::SubMenu("Gyroscope".to_string(), gyro));
        }

        let main_menu = Menu::new(rect, ViewId::MainMenu, MenuKind::DropDown, entries, context);
        rq.add(RenderData::new(main_menu.id(), *main_menu.rect(), UpdateMode::Gui));
        view.children_mut().push(Box::new(main_menu) as Box<dyn View>);
    }
}

pub fn toggle_battery_menu(view: &mut dyn View, rect: Rectangle, enable: Option<bool>, rq: &mut RenderQueue, context: &mut Context) {
    if let Some(index) = locate_by_id(view, ViewId::BatteryMenu) {
        if let Some(true) = enable {
            return;
        }
        rq.add(RenderData::expose(*view.child(index).rect(), UpdateMode::Gui));
        view.children_mut().remove(index);
    } else {
        if let Some(false) = enable {
            return;
        }

        let mut entries = Vec::new();

        match context.battery.status().ok().zip(context.battery.capacity().ok()) {
            Some((status, capacity)) => {
                for (i, (s, c)) in status.iter().zip(capacity.iter()).enumerate() {
                    entries.push(EntryKind::Message(format!("{:?} {}%", s, c),
                                                    if i > 0 { Some("cover".to_string()) } else { None }));
                }
            },
            _ => {
                entries.push(EntryKind::Message("Information Unavailable".to_string(), None));
            },
        }

        let battery_menu = Menu::new(rect, ViewId::BatteryMenu, MenuKind::DropDown, entries, context);
        rq.add(RenderData::new(battery_menu.id(), *battery_menu.rect(), UpdateMode::Gui));
        view.children_mut().push(Box::new(battery_menu) as Box<dyn View>);
    }
}

pub fn toggle_clock_menu(view: &mut dyn View, rect: Rectangle, enable: Option<bool>, rq: &mut RenderQueue, context: &mut Context) {
    if let Some(index) = locate_by_id(view, ViewId::ClockMenu) {
        if let Some(true) = enable {
            return;
        }
        rq.add(RenderData::expose(*view.child(index).rect(), UpdateMode::Gui));
        view.children_mut().remove(index);
    } else {
        if let Some(false) = enable {
            return;
        }
        let text = Local::now().format(&context.settings.date_format).to_string();
        let entries = vec![EntryKind::Message(text, None)];
        let clock_menu = Menu::new(rect, ViewId::ClockMenu, MenuKind::DropDown, entries, context);
        rq.add(RenderData::new(clock_menu.id(), *clock_menu.rect(), UpdateMode::Gui));
        view.children_mut().push(Box::new(clock_menu) as Box<dyn View>);
    }
}

pub fn toggle_input_history_menu(view: &mut dyn View, id: ViewId, rect: Rectangle, enable: Option<bool>, rq: &mut RenderQueue, context: &mut Context) {
    if let Some(index) = locate_by_id(view, ViewId::InputHistoryMenu) {
        if let Some(true) = enable {
            return;
        }
        rq.add(RenderData::expose(*view.child(index).rect(), UpdateMode::Gui));
        view.children_mut().remove(index);
    } else {
        if let Some(false) = enable {
            return;
        }
        let entries = context.input_history.get(&id)
                             .map(|h| h.iter().map(|s|
                                 EntryKind::Command(s.to_string(),
                                                    EntryId::SetInputText(id, s.to_string())))
                                           .collect::<Vec<EntryKind>>());
        if let Some(entries) = entries {
            let menu_kind = match id {
                ViewId::HomeSearchInput |
                ViewId::ReaderSearchInput |
                ViewId::DictionarySearchInput |
                ViewId::CalculatorInput => MenuKind::DropDown,
                _ => MenuKind::Contextual,
            };
            let input_history_menu = Menu::new(rect, ViewId::InputHistoryMenu, menu_kind, entries, context);
            rq.add(RenderData::new(input_history_menu.id(), *input_history_menu.rect(), UpdateMode::Gui));
            view.children_mut().push(Box::new(input_history_menu) as Box<dyn View>);
        }
    }
}

pub fn toggle_keyboard_layout_menu(view: &mut dyn View, rect: Rectangle, enable: Option<bool>, rq: &mut RenderQueue, context: &mut Context) {
    if let Some(index) = locate_by_id(view, ViewId::KeyboardLayoutMenu) {
        if let Some(true) = enable {
            return;
        }
        rq.add(RenderData::expose(*view.child(index).rect(), UpdateMode::Gui));
        view.children_mut().remove(index);
    } else {
        if let Some(false) = enable {
            return;
        }
        let entries = context.keyboard_layouts.keys()
                             .map(|s| EntryKind::Command(s.to_string(),
                                                         EntryId::SetKeyboardLayout(s.to_string())))
                             .collect::<Vec<EntryKind>>();
        let keyboard_layout_menu = Menu::new(rect, ViewId::KeyboardLayoutMenu, MenuKind::Contextual, entries, context);
        rq.add(RenderData::new(keyboard_layout_menu.id(), *keyboard_layout_menu.rect(), UpdateMode::Gui));
        view.children_mut().push(Box::new(keyboard_layout_menu) as Box<dyn View>);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn labels(entries: &[EntryKind]) -> Vec<String> {
        entries.iter().map(|e| match e {
            EntryKind::Command(label, _) => label.clone(),
            EntryKind::Separator => "---".to_string(),
            _ => "?".to_string(),
        }).collect()
    }

    #[test]
    fn every_app_is_offered_when_every_helper_is_installed() {
        assert_eq!(labels(&application_entries(|_| true)),
                   ["Dictionary", "Calculator", "Sketch", "---",
                    "Touch Events", "Rotation Values"]);
    }

    /// The case that started this: `ivy` is not in the payload.
    #[test]
    fn an_app_with_a_missing_helper_is_not_offered() {
        let entries = application_entries(|cmd| *cmd != AppCmd::Calculator);
        assert_eq!(labels(&entries),
                   ["Dictionary", "Sketch", "---", "Touch Events", "Rotation Values"]);
    }

    /// Filtering must not leave the rule it was separating from behind.
    #[test]
    fn a_menu_never_opens_or_ends_on_a_separator() {
        let only_tools = application_entries(|cmd| matches!(cmd, AppCmd::TouchEvents |
                                                                 AppCmd::RotationValues));
        assert_eq!(labels(&only_tools), ["Touch Events", "Rotation Values"]);

        let only_apps = application_entries(|cmd| matches!(cmd, AppCmd::Sketch));
        assert_eq!(labels(&only_apps), ["Sketch"]);

        assert!(application_entries(|_| false).is_empty());
    }

    #[test]
    fn separators_are_tidied_wherever_they_end_up() {
        let cmd = |label: &str| EntryKind::Command(label.to_string(), EntryId::About);
        let messy = vec![EntryKind::Separator, EntryKind::Separator, cmd("a"),
                         EntryKind::Separator, EntryKind::Separator, cmd("b"),
                         EntryKind::Separator];
        assert_eq!(labels(&tidy_separators(messy)), ["a", "---", "b"]);
        assert!(tidy_separators(vec![EntryKind::Separator]).is_empty());
        assert!(tidy_separators(Vec::new()).is_empty());
    }

    /// The mapping the probe rests on. Only the calculator shells out; every
    /// other app is built into the binary and can never be missing.
    #[test]
    fn only_the_calculator_needs_a_helper() {
        assert_eq!(AppCmd::Calculator.helper(), Some(PathBuf::from("bin/ivy/ivy")));
        assert!(AppCmd::Sketch.helper().is_none());
        assert!(AppCmd::TouchEvents.helper().is_none());
        assert!(AppCmd::RotationValues.helper().is_none());
        assert!(AppCmd::Dictionary { query: String::new(), language: String::new() }.helper().is_none());
        // Built-in apps are available whatever the filesystem says.
        assert!(AppCmd::Sketch.is_available());
    }
}
