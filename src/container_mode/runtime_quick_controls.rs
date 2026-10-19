use super::*;

pub(super) const RESERVED_HEIGHT: f64 = 280.0;
const CARD_HEIGHT: f64 = 242.0;
static SELECTIONS: LazyLock<Mutex<HashMap<usize, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static OPTIONS: Mutex<Vec<SelectedRuntimeContainer>> = Mutex::new(Vec::new());

fn options(nav: usize, rows: &[RuntimeContainerRow]) -> Vec<SelectedRuntimeContainer> {
    let runtime = if nav == NAV_APPLE_CONTAINER {
        "apple"
    } else {
        // OrbStack containers use the selected Docker-compatible endpoint.
        "docker"
    };
    rows.iter()
        .filter_map(|row| {
            Some(SelectedRuntimeContainer {
                runtime: runtime.into(),
                name: row.name.as_ref()?.clone(),
                label: row.label.clone(),
                running: row.running,
            })
        })
        .collect()
}

fn selected_index(options: &[SelectedRuntimeContainer], name: Option<&str>) -> Option<usize> {
    options
        .iter()
        .position(|row| Some(row.name.as_str()) == name)
        .or_else(|| (!options.is_empty()).then_some(0))
}

fn button_frames(width: f64, bottom: f64) -> [NSRect; 4] {
    let button_width = (width - 86.0) / 2.0;
    std::array::from_fn(|index| {
        rect(
            38.0 + (index % 2) as f64 * (button_width + 10.0),
            bottom + if index < 2 { 56.0 } else { 18.0 },
            button_width,
            30.0,
        )
    })
}

pub(super) fn select(sender: &AnyObject) {
    let (nav, index): (isize, isize) = unsafe {
        (
            msg_send![sender, tag],
            msg_send![sender, indexOfSelectedItem],
        )
    };
    if index < 0
        || !matches!(
            nav as usize,
            NAV_APPLE_CONTAINER | NAV_DOCKER | NAV_ORBSTACK
        )
    {
        return;
    }
    let selection = OPTIONS.lock().unwrap().get(index as usize).cloned();
    if let Some(selection) = selection {
        SELECTIONS
            .lock()
            .unwrap()
            .insert(nav as usize, selection.name);
        // Changing the shortcut target must not steal focus or jump to details.
        unsafe { refresh_window_without_focus() };
    }
}

pub(super) fn add(
    parent: &NSView,
    width: f64,
    y: f64,
    nav: usize,
    rows: &[RuntimeContainerRow],
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) -> f64 {
    let bottom = y + 18.0 - CARD_HEIGHT;
    add_card(parent, rect(24.0, bottom, width - 48.0, CARD_HEIGHT), mtm);
    add_label(
        parent,
        &tr("Container Quick Controls"),
        rect(38.0, bottom + 206.0, width - 76.0, 22.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        &tr("Controls container lifecycle, not application launch."),
        rect(38.0, bottom + 166.0, width - 76.0, 34.0),
        mtm,
        TextStyle::Caption,
    );
    let options = options(nav, rows);
    let index = {
        let mut selections = SELECTIONS.lock().unwrap();
        let index = selected_index(&options, selections.get(&nav).map(String::as_str));
        if let Some(index) = index {
            selections.insert(nav, options[index].name.clone());
        } else {
            selections.remove(&nav);
        }
        index
    };
    let names: Vec<&str> = if options.is_empty() {
        vec!["—"]
    } else {
        options.iter().map(|row| row.name.as_str()).collect()
    };
    let popup = add_popup(
        parent,
        rect(38.0, bottom + 126.0, width - 76.0, 30.0),
        &names,
        index.unwrap_or(0),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*popup, setTarget: handler];
        let _: () = msg_send![&*popup, setAction: sel!(selectQuickRuntimeContainer:)];
        let _: () = msg_send![&*popup, setTag: nav as isize];
        let _: () = msg_send![&*popup, setEnabled: index.is_some()];
        let _: () = msg_send![&*popup, setToolTip: &*NSString::from_str(&tr("Select a container"))];
    }
    let selected = index.map(|index| &options[index]);
    let running = selected.is_some_and(|row| row.running);
    add_label(
        parent,
        &tr(match selected {
            Some(row) if row.running => "Running",
            Some(_) => "Stopped",
            None => "No actionable containers. Reload after starting the runtime.",
        }),
        rect(38.0, bottom + 92.0, width - 76.0, 30.0),
        mtm,
        TextStyle::Caption,
    );
    let action_index = selected.map(|row| push_runtime_container_action(&row.runtime, &row.name));
    let detail_index = selected.map(|row| {
        let mut actions = RUNTIME_CONTAINER_SELECT_ACTIONS.lock().unwrap();
        let index = actions.len();
        actions.push(row.clone());
        index
    });
    let actions = [
        (
            if running { "Stop" } else { "Start" },
            if running {
                sel!(stopRuntimeContainer:)
            } else {
                sel!(startRuntimeContainer:)
            },
            action_index,
            selected.is_some(),
        ),
        (
            "Restart",
            sel!(restartRuntimeContainer:),
            action_index,
            running,
        ),
        (
            "Terminal",
            sel!(openRuntimeContainerTerminal:),
            action_index,
            running,
        ),
        (
            "Details",
            sel!(selectRuntimeContainer:),
            detail_index,
            selected.is_some(),
        ),
    ];
    for ((title, action, tag, enabled), frame) in
        actions.into_iter().zip(button_frames(width, bottom))
    {
        let button = add_button(parent, title, frame, handler, action, mtm);
        unsafe {
            let _: () = msg_send![&*button, setTag: tag.map_or(-1, |index| index as isize)];
            let _: () = msg_send![&*button, setEnabled: enabled];
        }
    }
    *OPTIONS.lock().unwrap() = options;
    y - RESERVED_HEIGHT
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: Option<&str>, running: bool) -> RuntimeContainerRow {
        RuntimeContainerRow {
            name: name.map(str::to_owned),
            label: "raw label".into(),
            running,
        }
    }

    #[test]
    fn notices_and_helpers_are_not_action_targets() {
        let items = options(
            NAV_APPLE_CONTAINER,
            &[row(None, false), row(Some("vm"), true)],
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].runtime, "apple");
        assert_eq!(items[0].name, "vm");
        assert!(items[0].running);
        assert!(options(NAV_DOCKER, &[row(None, false)]).is_empty());
        assert_eq!(
            options(NAV_ORBSTACK, &[row(Some("vm"), false)])[0].runtime,
            "docker"
        );
    }

    #[test]
    fn selection_tracks_identity_not_inventory_order() {
        let items = options(NAV_DOCKER, &[row(Some("b"), false), row(Some("a"), true)]);
        assert_eq!(selected_index(&items, Some("a")), Some(1));
        assert_eq!(selected_index(&items, Some("removed")), Some(0));
        assert_eq!(selected_index(&[], Some("a")), None);
    }

    #[test]
    fn shortcuts_include_containers_beyond_inventory_preview() {
        let rows: Vec<_> = (0..24)
            .map(|i| row(Some(&format!("vm-{i}")), false))
            .collect();
        let items = options(NAV_APPLE_CONTAINER, &rows);
        assert_eq!(items.len(), 24);
        assert_eq!(selected_index(&items, Some("vm-23")), Some(23));
    }

    #[test]
    fn narrow_and_wide_button_frames_stay_in_card_without_overlap() {
        for width in [LIST_MIN_WIDTH, 400.0, 520.0, 900.0] {
            let frames = button_frames(width, 100.0);
            for frame in frames {
                assert!(frame.size.width >= 129.0);
                assert!(frame.origin.x >= 38.0);
                assert!(frame.origin.x + frame.size.width <= width - 38.0);
                assert!(frame.origin.y >= 118.0);
                assert!(frame.origin.y + frame.size.height <= 186.0);
            }
            assert!(frames[0].origin.x + frames[0].size.width < frames[1].origin.x);
            assert!(frames[2].origin.y + frames[2].size.height < frames[0].origin.y);
        }
        assert!(RESERVED_HEIGHT > CARD_HEIGHT + 18.0);
    }
}
