//! Runs the compositor without a backend and talks to it as a real Wayland client. Input is
//! injected where a backend would deliver it, output is rendered with pixman. The harness is
//! `wirbel::test_harness`.

use wirbel::test_harness::*;

#[test]
fn offers_the_globals_kitty_needs() {
    let compositor = Compositor::start();
    let client = Client::connect(&compositor);
    let names: Vec<String> = client
        .globals
        .contents()
        .clone_list()
        .into_iter()
        .map(|global| global.interface)
        .collect();
    for name in [
        "wl_compositor",
        "wl_shm",
        "wl_seat",
        "wl_output",
        "xdg_wm_base",
        "zwlr_layer_shell_v1",
        "zxdg_decoration_manager_v1",
        "zxdg_output_manager_v1",
        "wl_data_device_manager",
        "zwp_primary_selection_device_manager_v1",
        "wp_cursor_shape_manager_v1",
        "wp_viewporter",
        "wp_fractional_scale_manager_v1",
        "ext_session_lock_manager_v1",
        "zwp_idle_inhibit_manager_v1",
        "ext_idle_notifier_v1",
        "zwp_pointer_constraints_v1",
        "zwp_relative_pointer_manager_v1",
        "wp_presentation",
        "xdg_activation_v1",
    ] {
        assert!(
            names.iter().any(|global| global == name),
            "{name} missing in {names:?}"
        );
    }
    client.connection.flush().unwrap();
}

#[test]
fn windows_are_tiled_and_focused_as_they_open_and_close() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);

    let a = client.open_window(0x00ff_0000);
    assert_eq!(client.size(a), FULL);
    assert!(client.activated(a));
    assert_eq!(client.take_input(), [Input::KeyboardEnter(a)]);

    let b = client.open_window(0x0000_ff00);
    assert_eq!(client.size(a), LEFT);
    assert_eq!(client.size(b), RIGHT);
    assert!(!client.activated(a));
    assert!(client.activated(b));
    assert_eq!(
        client.take_input(),
        [Input::KeyboardLeave(a), Input::KeyboardEnter(b)]
    );

    client.close(b);
    assert_eq!(client.size(a), FULL);
    assert!(client.activated(a));
    assert_eq!(client.take_input(), [Input::KeyboardEnter(a)]);
    assert!(compositor.with(|state| state.monitors[0].workspace().tiled.items().len() == 1));
}

#[test]
fn window_gets_no_focus_before_its_first_buffer() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_unmapped_window();
    assert_eq!(client.size(a), FULL);
    assert_eq!(client.take_input(), []);
    client.draw(a, 0x00ff_0000);
    assert_eq!(client.take_input(), [Input::KeyboardEnter(a)]);
}

#[test]
fn keys_reach_only_the_focused_window() {
    let compositor = Compositor::start();
    let mut other = Client::connect(&compositor);
    let mut client = Client::connect(&compositor);
    let _ = other.open_window(0x00ff_0000);
    let b = client.open_window(0x0000_ff00);
    other.take_input();
    client.take_input();

    const KEY_A: u32 = 30;
    compositor.tap_key(KEY_A);
    assert_eq!(
        client.take_input(),
        [
            Input::Key(KEY_A, wl_keyboard::KeyState::Pressed),
            Input::Key(KEY_A, wl_keyboard::KeyState::Released),
        ]
    );
    assert_eq!(other.take_input(), []);
    assert!(client.activated(b));
}

#[test]
fn pointer_enters_with_surface_local_coordinates() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    let b = client.open_window(0x0000_ff00);
    client.draw(a, 0x00ff_0000);
    client.take_input();

    compositor.pointer_at(114.0, 44.5);
    assert_eq!(client.take_input(), [Input::PointerEnter(a, 100.0, 30.5)]);

    compositor.pointer_at(115.0, 44.5);
    assert_eq!(client.take_input(), [Input::PointerMotion(101.0, 30.5)]);

    compositor.pointer_at(RIGHT_X + 3.0, 50.0);
    assert_eq!(
        client.take_input(),
        [Input::PointerLeave(a), Input::PointerEnter(b, 3.0, 36.0)]
    );

    // The gap between the windows belongs to nobody.
    compositor.pointer_at(500.0, 50.0);
    assert_eq!(client.take_input(), [Input::PointerLeave(b)]);
}

#[test]
fn click_moves_focus_and_reaches_the_client() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    let b = client.open_window(0x0000_ff00);
    compositor.pointer_at(100.0, 100.0);
    client.take_input();
    assert!(client.activated(b));

    compositor.click();
    assert_eq!(
        client.take_input(),
        [
            Input::KeyboardLeave(b),
            Input::KeyboardEnter(a),
            Input::Button(BTN_LEFT, wl_pointer::ButtonState::Pressed),
            Input::Button(BTN_LEFT, wl_pointer::ButtonState::Released),
        ]
    );
    assert!(client.activated(a));
    assert!(!client.activated(b));
}

#[test]
fn clicking_the_background_keeps_focus() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    compositor.pointer_at(2.0, 2.0);
    compositor.click();
    assert!(client.activated(a));
    assert_eq!(client.take_input(), [Input::KeyboardEnter(a)]);
}

#[test]
fn pointer_focus_follows_windows_that_move_under_it() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    compositor.pointer_at(800.0, 300.0);
    assert_eq!(
        client.take_input(),
        [
            Input::KeyboardEnter(a),
            Input::PointerEnter(a, 786.0, 286.0)
        ]
    );

    // The new window takes the right half, where the pointer rests.
    let b = client.open_window(0x0000_ff00);
    let input = client.take_input();
    assert!(input.contains(&Input::PointerLeave(a)), "{input:?}");
    assert!(
        input.contains(&Input::PointerEnter(b, 800.0 - RIGHT_X, 286.0)),
        "{input:?}"
    );
}

#[test]
fn pointer_enters_a_window_that_grows_under_it() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    let b = client.open_window(0x0000_ff00);
    client.draw(a, 0x00ff_0000);
    compositor.pointer_at(800.0, 300.0);
    client.take_input();

    client.close(b);
    assert_eq!(client.take_input(), [Input::KeyboardEnter(a)]);

    client.draw(a, 0x00ff_0000);
    assert_eq!(client.take_input(), [Input::PointerEnter(a, 786.0, 286.0)]);

    // Further commits from the same window are not pointer events.
    client.draw(a, 0x00ff_0000);
    assert_eq!(client.take_input(), []);
}

#[test]
fn output_resize_retiles() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        let mode = Mode {
            size: (600, 400).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, None);
        state.output_resized(&output);
    });
    client.sync();
    assert_eq!(client.size(a), (572, 372));
}

#[test]
fn null_buffer_unmaps_and_the_window_can_return() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    let b = client.open_window(0x0000_ff00);
    client.take_input();

    let surface = client.state.windows[b].surface.clone();
    surface.attach(None, 0, 0);
    surface.commit();
    client.sync();
    assert_eq!(client.size(a), FULL);
    assert_eq!(
        client.take_input(),
        [Input::KeyboardLeave(b), Input::KeyboardEnter(a)]
    );
    assert!(compositor.with(|state| state.monitors[0].workspace().tiled.items().len() == 1));

    surface.commit();
    client.sync();
    assert_eq!(client.size(a), LEFT);
    assert_eq!(client.size(b), RIGHT);
    client.draw(b, 0x0000_ff00);
    assert_eq!(
        client.take_input(),
        [Input::KeyboardLeave(a), Input::KeyboardEnter(b)]
    );
}

#[test]
fn disconnecting_removes_all_windows_of_the_client() {
    let compositor = Compositor::start();
    let mut survivor = Client::connect(&compositor);
    let a = survivor.open_window(0x00ff_0000);
    {
        let mut client = Client::connect(&compositor);
        client.open_window(0x0000_ff00);
        client.open_window(0x0000_00ff);
        survivor.sync();
        assert!(compositor.with(|state| state.monitors[0].workspace().tiled.items().len() == 3));
    }
    survivor.sync();
    // The server sees the hangup asynchronously.
    for _ in 0..100 {
        if compositor.with(|state| state.monitors[0].workspace().tiled.items().len() == 1) {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(10));
        survivor.sync();
    }
    // The loop may end right after the state changed, before the configure got here.
    survivor.sync();
    assert_eq!(survivor.size(a), FULL);
    assert!(survivor.activated(a));
}

#[test]
fn popups_stay_on_the_output_and_are_drawn_above_their_window() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);

    let (surface, _popup) = client.open_edge_popup(a);

    // The window starts at x=14, so x=886 puts the right edge exactly on the 1000 wide output.
    assert_eq!(client.state.popup_position, Some((886, 21)));

    let _buffer = client.fill(&surface, 100, 50, 0x0000_00ff);
    let shot = compositor.screenshot();
    assert_eq!(shot.at(14 + 886, 14 + 21), 0x00_00ff);
    assert_eq!(shot.at(999, 14 + 70), 0x00_00ff);
    assert_eq!(shot.at(14 + 885, 14 + 21), 0xff_0000);
}

#[test]
fn renders_windows_borders_and_background() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(0x00ff_0000);
    client.open_window(0x0000_ff00);
    // The first window was resized by the second one and has to catch up.
    client.draw(0, 0x00ff_0000);

    let shot = compositor.screenshot();
    let background = rgb(Config::default().background_color);
    let border = rgb(Config::default().border_color);
    let focus = rgb(Config::default().focus_color);

    assert_eq!(shot.at(0, 0), background, "outer gap");
    assert_eq!(shot.at(10, 10), border, "corner of the first tile");
    assert_eq!(shot.at(13, 300), border, "left border");
    assert_eq!(shot.at(14, 300), 0xff_0000, "first window");
    assert_eq!(shot.at(493, 300), 0xff_0000, "first window, last column");
    assert_eq!(shot.at(494, 300), border, "right border of the first tile");
    assert_eq!(shot.at(500, 300), background, "inner gap");
    assert_eq!(shot.at(503, 300), focus, "border of the focused tile");
    assert_eq!(shot.at(507, 300), 0x00_ff00, "second window");
    assert_eq!(shot.at(985, 300), 0x00_ff00, "second window, last column");
    assert_eq!(shot.at(986, 300), focus, "right border");
    assert_eq!(shot.at(989, 589), focus, "corner of the second tile");
    assert_eq!(shot.at(990, 590), background, "outer gap at the far corner");
}

#[test]
fn border_follows_focus() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    client.open_window(0x0000_ff00);
    client.draw(a, 0x00ff_0000);
    compositor.pointer_at(100.0, 100.0);
    compositor.click();
    client.sync();

    let shot = compositor.screenshot();
    assert_eq!(shot.at(10, 300), rgb(Config::default().focus_color));
    assert_eq!(shot.at(503, 300), rgb(Config::default().border_color));
}

#[test]
fn a_bound_key_never_reaches_the_client() {
    let (compositor, mut client, _, _) = two_windows();
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    client.take_input();
    compositor.tap_key(KEY_2);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    // The window lost focus through the bind, but no key event got out.
    assert!(
        !client
            .take_input()
            .iter()
            .any(|input| matches!(input, Input::Key(code, _) if *code == KEY_2)),
    );
    assert_eq!(compositor.active_workspace(), 1);
}

#[test]
fn an_unbound_combination_reaches_the_client() {
    let (compositor, mut client, _, _) = two_windows();
    compositor.chord(SUPER, KEY_Q);
    assert_eq!(
        client.take_input(),
        [
            Input::Key(KEY_LEFTMETA, wl_keyboard::KeyState::Pressed),
            Input::Key(KEY_Q, wl_keyboard::KeyState::Pressed),
            Input::Key(KEY_Q, wl_keyboard::KeyState::Released),
            Input::Key(KEY_LEFTMETA, wl_keyboard::KeyState::Released),
        ]
    );
}

#[test]
fn a_key_released_after_its_modifier_stays_swallowed() {
    let (compositor, mut client, _, _) = two_windows();
    compositor.key(KEY_LEFTALT, KeyState::Pressed);
    compositor.key(KEY_LEFT, KeyState::Pressed);
    compositor.key(KEY_LEFTALT, KeyState::Released);
    compositor.key(KEY_LEFT, KeyState::Released);
    assert!(
        !client
            .take_input()
            .iter()
            .any(|input| matches!(input, Input::Key(code, _) if *code == KEY_LEFT))
    );
    // And the same key is an ordinary key again afterwards.
    compositor.tap_key(KEY_LEFT);
    assert!(
        client
            .take_input()
            .contains(&Input::Key(KEY_LEFT, wl_keyboard::KeyState::Pressed))
    );
}

#[test]
fn spawn_runs_the_command_of_the_bind() {
    let marker = scratch_path("spawned");
    let compositor = Compositor::start_with(&format!(
        "bind = SUPER, Return, spawn, touch {}\n",
        marker.display()
    ));
    compositor.chord(SUPER, KEY_ENTER);
    for _ in 0..200 {
        if marker.exists() {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(marker.exists());
    fs::remove_file(marker).unwrap();
}

#[test]
fn spawned_programs_start_at_home() {
    let marker = scratch_path("spawned-cwd");
    let compositor = Compositor::start_with(&format!(
        "bind = SUPER, Return, spawn, pwd > {}\n",
        marker.display()
    ));
    compositor.chord(SUPER, KEY_ENTER);
    for _ in 0..200 {
        if fs::read_to_string(&marker).is_ok_and(|text| text.ends_with('\n')) {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
    let cwd = fs::read_to_string(&marker).unwrap();
    fs::remove_file(marker).unwrap();
    let home = fs::canonicalize(std::env::var("HOME").unwrap()).unwrap();
    assert_eq!(fs::canonicalize(cwd.trim()).unwrap(), home);
}

#[test]
fn close_asks_the_focused_window_only() {
    let (compositor, mut client, _, b) = two_windows();
    compositor.chord(SUPER, KEY_W);
    client.sync();
    assert_eq!(client.state.closed, [b]);
}

#[test]
fn quit_ends_the_event_loop() {
    let mut compositor = Compositor::start();
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    // The job that runs this is the last thing the loop does.
    let keycode = 50u32 + 8; // M
    let job: Job = Box::new(move |state| {
        state.on_key(keycode.into(), KeyState::Pressed, InputTime::now());
    });
    compositor.jobs.send(job).unwrap();
    compositor.thread.take().unwrap().join().unwrap();
}

#[test]
fn the_default_keymap_is_german() {
    let compositor = Compositor::start_with("bind = SUPER, z, focus_next\n");
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_window(GREEN);
    client.take_input();
    let before = compositor.focused_rect();
    compositor.chord(SUPER, KEY_Y);
    assert_ne!(compositor.focused_rect(), before);
}

#[test]
fn the_keymap_comes_from_the_config() {
    let compositor = Compositor::start_with("keyboard_layout = us\nbind = SUPER, y, focus_next\n");
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_window(GREEN);
    let before = compositor.focused_rect();
    compositor.chord(SUPER, KEY_Y);
    assert_ne!(compositor.focused_rect(), before);
}

#[test]
fn a_shifted_digit_matches_by_its_unshifted_symbol() {
    let (compositor, _client, _, _) = two_windows();
    // On a German keyboard Shift+2 produces a quotation mark.
    compositor.chord(SUPER_SHIFT, KEY_2);
    assert_eq!(compositor.active_workspace(), 1);
    assert_eq!(compositor.frames().len(), 1);
}

#[test]
fn the_keyboard_repeat_comes_from_the_config() {
    let compositor = Compositor::start_with("repeat_delay = 123\nrepeat_rate = 45\n");
    let client = Client::connect(&compositor);
    assert_eq!(client.state.repeat, Some((45, 123)));
}

#[test]
fn reload_applies_a_changed_file_to_the_running_compositor() {
    let path = scratch_path("reload");
    fs::write(&path, "gap_outer = 0\n").unwrap();
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    assert_eq!(client.size(a), FULL);

    let config_path = path.clone();
    compositor.with(move |state| state.config_path = Some(config_path));
    compositor.chord(SUPER, KEY_R);
    client.sync();
    assert_eq!(client.size(a), (992, 592));

    // Binds and keymap are live as well.
    fs::write(
        &path,
        "keyboard_layout = us\nrepeat_rate = 7\nbind = SUPER, y, float\n",
    )
    .unwrap();
    compositor.chord(SUPER, KEY_R);
    assert_eq!(compositor.with(|state| state.config.binds.len()), 2);
    client.sync();
    assert_eq!(client.state.repeat, Some((7, 600)));
    compositor.chord(SUPER, KEY_Y);
    assert_eq!(is_tiled(&compositor), [false]);
    fs::remove_file(path).unwrap();
}

#[test]
fn reload_keeps_the_running_config_if_the_new_one_is_broken() {
    let path = scratch_path("reload-broken");
    let compositor = Compositor::start();
    let config_path = path.clone();
    compositor.with(move |state| state.config_path = Some(config_path));
    let before = compositor.with(|state| state.config.clone());

    for text in [
        "gap_outer = 3\nwobble\n",
        "gap_outer = 3\nkeyboard_layout = nosuchlayout\n",
    ] {
        fs::write(&path, text).unwrap();
        compositor.chord(SUPER, KEY_R);
        let config = compositor.with(|state| state.config.clone());
        assert_eq!(config, before, "{text}");
    }

    // The keymap in use survives the failed attempt.
    assert_eq!(
        compositor.with(|state| {
            let keyboard = state.seat.get_keyboard().unwrap();
            keyboard.with_xkb_state(state, |context| {
                let xkb = context.xkb().lock().unwrap();
                xkb.layout_name(xkb.active_layout()).to_string()
            })
        }),
        "German"
    );
    fs::remove_file(path).unwrap();

    // A deleted file means the defaults, as it does at startup.
    compositor.chord(SUPER, KEY_R);
    assert_eq!(
        compositor.with(|state| state.config.clone()),
        Config::default()
    );
}

#[test]
fn reload_without_a_path_does_nothing() {
    let compositor = Compositor::start();
    let before = compositor.with(|state| state.config.clone());
    compositor.chord(SUPER, KEY_R);
    assert_eq!(compositor.with(|state| state.config.clone()), before);
}

#[test]
fn focus_moves_by_direction_and_stops_at_the_edge() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    // a on the left, b top right, c bottom right.
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);
    let c = client.open_window(BLUE);
    client.sync();
    assert!(client.activated(c));

    compositor.chord(ALT, KEY_UP);
    client.sync();
    assert!(client.activated(b));
    compositor.chord(ALT, KEY_LEFT);
    client.sync();
    assert!(client.activated(a));
    compositor.chord(ALT, KEY_LEFT);
    client.sync();
    assert!(client.activated(a));
    compositor.chord(ALT, KEY_RIGHT);
    client.sync();
    assert!(client.activated(b));
    compositor.chord(ALT, KEY_DOWN);
    client.sync();
    assert!(client.activated(c));
}

#[test]
fn focus_next_cycles_through_the_workspace() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(SUPER, KEY_TAB);
    client.sync();
    assert!(client.activated(a));
    compositor.chord(SUPER, KEY_TAB);
    client.sync();
    assert!(client.activated(b));
}

#[test]
fn swap_exchanges_tiles_and_keeps_focus_on_the_window() {
    let (compositor, mut client, a, b) = two_windows();
    let frames = compositor.frames();
    compositor.chord(SUPER_SHIFT, KEY_LEFT);
    client.sync();
    assert_eq!(client.size(b), LEFT);
    assert_eq!(client.size(a), RIGHT);
    assert!(client.activated(b));
    assert_eq!(compositor.focused_rect(), Some(frames[0]));

    // Nothing further left to swap with.
    compositor.chord(SUPER_SHIFT, KEY_LEFT);
    client.sync();
    assert_eq!(client.size(b), LEFT);
}

#[test]
fn split_ratio_grows_the_focused_tile_from_either_side() {
    let (compositor, mut client, a, b) = two_windows();
    let right = &[KEY_LEFTCTRL, KEY_LEFTALT];
    compositor.chord(right, KEY_RIGHT);
    client.sync();
    assert!(client.size(b).0 > RIGHT.0, "{:?}", client.size(b));
    assert!(client.size(a).0 < LEFT.0);

    compositor.chord(right, KEY_LEFT);
    compositor.chord(right, KEY_LEFT);
    client.sync();
    assert!(client.size(b).0 < RIGHT.0);
    assert!(client.size(a).0 > LEFT.0);
}

#[test]
fn split_ratio_ignores_a_single_window() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    compositor.chord(&[KEY_LEFTCTRL, KEY_LEFTALT], KEY_RIGHT);
    client.sync();
    assert_eq!(client.size(a), FULL);
}

#[test]
fn workspaces_hide_and_restore_their_windows() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(SUPER, KEY_2);
    client.sync();
    assert!(compositor.frames().is_empty());
    assert_eq!(compositor.with(|state| state.space.elements().count()), 0);
    assert_eq!(compositor.focused_rect(), None);
    assert!(client.take_input().contains(&Input::KeyboardLeave(b)));

    // Workspace 2 tiles on its own.
    let c = client.open_window(BLUE);
    assert_eq!(client.size(c), FULL);
    assert_eq!(client.size(a), LEFT);
    let shot = compositor.screenshot();
    assert_eq!(shot.at(250, 300), BLUE & 0xff_ffff);

    compositor.chord(SUPER, KEY_1);
    client.sync();
    assert!(client.activated(b), "focus did not come back");
    assert!(!client.activated(c));
    assert_eq!(compositor.frames().len(), 2);
    let shot = compositor.screenshot();
    assert_eq!(shot.at(250, 300), RED & 0xff_ffff);
    assert_eq!(shot.at(750, 300), GREEN & 0xff_ffff);
}

#[test]
fn input_does_not_reach_a_hidden_window() {
    let (compositor, mut client, _, _) = two_windows();
    compositor.pointer_at(250.0, 300.0);
    compositor.chord(SUPER, KEY_2);
    client.take_input();
    compositor.pointer_at(260.0, 310.0);
    compositor.click();
    compositor.tap_key(KEY_Q);
    assert_eq!(client.take_input(), []);
}

#[test]
fn a_window_on_a_hidden_workspace_keeps_living() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(SUPER, KEY_2);
    client.sync();

    // It may close there, and its workspace rearranges for the next visit.
    client.close(b);
    compositor.chord(SUPER, KEY_1);
    client.sync();
    assert_eq!(client.size(a), FULL);
    assert!(client.activated(a));

    // Commits of a hidden window must not disturb anything.
    compositor.chord(SUPER, KEY_2);
    client.sync();
    client.draw(a, BLUE);
    assert_eq!(compositor.frames().len(), 0);
    compositor.chord(SUPER, KEY_1);
    assert_eq!(compositor.screenshot().at(250, 300), BLUE);
}

#[test]
fn a_window_whose_first_buffer_arrives_on_a_hidden_workspace_takes_no_focus() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_unmapped_window();
    compositor.chord(SUPER, KEY_2);
    let b = client.open_window(GREEN);
    client.draw(a, RED);
    client.sync();
    assert!(client.activated(b));
    assert!(!client.activated(a));

    compositor.chord(SUPER, KEY_1);
    client.sync();
    assert!(client.activated(a), "it should be next in line there");
}

#[test]
fn workspace_prev_and_next_wrap_around() {
    let compositor = Compositor::start();
    compositor.chord(SUPER, KEY_LEFT);
    assert_eq!(compositor.active_workspace(), 8);
    compositor.chord(SUPER, KEY_RIGHT);
    assert_eq!(compositor.active_workspace(), 0);
}

#[test]
fn move_to_workspace_takes_the_window_along() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(SUPER_SHIFT, KEY_3);
    client.sync();
    assert_eq!(compositor.active_workspace(), 2);
    assert_eq!(client.size(b), FULL);
    assert!(client.activated(b));

    compositor.chord(SUPER, KEY_1);
    client.sync();
    assert_eq!(client.size(a), FULL);
    assert!(client.activated(a));

    // Into its own workspace it goes nowhere.
    compositor.chord(SUPER_SHIFT, KEY_1);
    assert_eq!(compositor.active_workspace(), 0);
    assert_eq!(compositor.frames().len(), 1);
}

#[test]
fn float_toggles_out_of_and_back_into_the_tree() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(ALT, 12); // ß
    client.sync();
    assert_eq!(client.size(a), FULL, "the tile should take the free space");
    assert_eq!(is_tiled(&compositor), [true, false]);
    // Its width is below the limit of 3/4 of the output and stays, its height is cut.
    assert_eq!(client.size(b), (RIGHT.0, 450));
    client.draw(b, GREEN);
    let frame = compositor.focused_rect().unwrap();
    assert_eq!((frame.w, frame.h), (RIGHT.0, 450));
    assert_eq!(frame.center(), (500, 300));

    compositor.chord(ALT, 12);
    client.sync();
    assert_eq!(is_tiled(&compositor), [true, true]);
    assert_eq!(client.size(a), LEFT);
    assert_eq!(client.size(b), RIGHT);
}

#[test]
fn a_big_window_shrinks_when_it_starts_floating() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    compositor.chord(ALT, 12);
    client.sync();
    assert_eq!(client.size(a), (750, 450));
}

#[test]
fn floating_windows_stay_above_tiles_and_cover_their_borders() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(ALT, 12);
    client.sync();
    client.draw(a, RED);
    client.draw(b, GREEN);

    // Focusing the tile must not bury the floating window.
    compositor.chord(SUPER, KEY_TAB);
    client.sync();
    assert!(client.activated(a));
    let shot = compositor.screenshot();
    assert_eq!(shot.at(500, 300), GREEN);

    // The border of the tile runs along the bottom, the floating window must hide it if it
    // reaches there. Its own border is drawn above the tile.
    let frame = compositor.frames()[1];
    assert_eq!(
        shot.at(frame.x - 2, 300),
        rgb(Config::default().border_color)
    );
}

#[test]
fn dialogs_and_fixed_size_windows_float_centred() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let main = client.open_window(RED);
    let parent = client.toplevel(main);
    let dialog = client.open_sized_window((300, 200), GREEN, |toplevel| {
        toplevel.set_parent(Some(&parent));
    });
    client.sync();
    assert_eq!(client.size(main), FULL, "the dialog should not be tiled");
    assert!(client.activated(dialog));
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(350, 200, 300, 200))
    );
    assert_eq!(compositor.screenshot().at(500, 300), GREEN);
    assert_eq!(
        compositor.screenshot().at(348, 300),
        rgb(Config::default().focus_color)
    );

    let fixed = client.open_sized_window((100, 100), BLUE, |toplevel| {
        toplevel.set_min_size(100, 100);
        toplevel.set_max_size(100, 100);
    });
    client.sync();
    assert_eq!(client.size(main), FULL);
    assert!(client.activated(fixed));
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(450, 250, 100, 100))
    );
}

#[test]
fn a_floating_window_may_resize_itself() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let main = client.open_window(RED);
    let parent = client.toplevel(main);
    let dialog = client.open_sized_window((300, 200), GREEN, |toplevel| {
        toplevel.set_parent(Some(&parent));
    });
    client.draw_sized(dialog, (400, 100), GREEN);
    client.sync();
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(350, 200, 400, 100))
    );
    let shot = compositor.screenshot();
    assert_eq!(shot.at(748, 250), GREEN);
    assert_eq!(shot.at(752, 250), rgb(Config::default().focus_color));
    // A later retile must not force the old size back on it.
    compositor.with(|state| state.retile());
    client.sync();
    assert_eq!(client.size(dialog), (400, 100));
}

#[test]
fn fullscreen_covers_the_output_and_hides_the_rest() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(ALT, KEY_F);
    client.sync();
    assert_eq!(client.size(b), (WIDTH, HEIGHT));
    assert!(client.has_state(b, xdg_toplevel::State::Fullscreen));
    client.draw(b, GREEN);
    assert_eq!(compositor.frames().len(), 1);
    assert_eq!(compositor.with(|state| state.borders.len()), 0);
    let shot = compositor.screenshot();
    for (x, y) in [(0, 0), (999, 599), (500, 300), (250, 300)] {
        assert_eq!(shot.at(x, y), GREEN, "{x},{y}");
    }

    compositor.chord(ALT, KEY_F);
    client.sync();
    assert_eq!(client.size(b), RIGHT);
    assert_eq!(client.size(a), LEFT);
    assert!(!client.has_state(b, xdg_toplevel::State::Fullscreen));
    assert_eq!(compositor.frames().len(), 2);
}

#[test]
fn maximize_fills_the_output_inside_the_outer_gap() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(ALT, KEY_A);
    client.sync();
    assert_eq!(client.size(b), FULL);
    assert!(client.has_state(b, xdg_toplevel::State::Maximized));
    client.draw(b, GREEN);
    let shot = compositor.screenshot();
    assert_eq!(shot.at(500, 300), GREEN);
    assert_eq!(shot.at(12, 12), rgb(Config::default().focus_color));
    assert_eq!(shot.at(5, 5), rgb(Config::default().background_color));

    // Fullscreen takes over from maximize and is left in one step.
    compositor.chord(ALT, KEY_F);
    client.sync();
    assert_eq!(client.size(b), (WIDTH, HEIGHT));
    assert!(!client.has_state(b, xdg_toplevel::State::Maximized));
    compositor.chord(ALT, KEY_F);
    client.sync();
    assert_eq!(client.size(b), RIGHT);
    assert_eq!(client.size(a), LEFT);
}

#[test]
fn clients_can_ask_for_fullscreen_and_maximize() {
    let (_compositor, mut client, a, b) = two_windows();
    client.toplevel(a).set_fullscreen(None);
    client.sync();
    assert_eq!(client.size(a), (WIDTH, HEIGHT));
    assert!(client.activated(a), "the covering window needs the focus");
    assert!(!client.activated(b));

    client.toplevel(a).unset_fullscreen();
    client.sync();
    assert_eq!(client.size(a), LEFT);

    client.toplevel(b).set_maximized();
    client.sync();
    assert_eq!(client.size(b), FULL);
    assert!(client.has_state(b, xdg_toplevel::State::Maximized));
    client.toplevel(b).unset_maximized();
    client.sync();
    assert_eq!(client.size(b), RIGHT);

    // A redundant request still gets its configure.
    let before = client.state.windows[b].configures;
    client.toplevel(b).unset_maximized();
    client.sync();
    assert!(client.state.windows[b].configures > before);
}

#[test]
fn a_window_can_start_fullscreen() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_unmapped_window_with(|toplevel| toplevel.set_fullscreen(None));
    assert_eq!(client.size(b), (WIDTH, HEIGHT));
    assert!(client.has_state(b, xdg_toplevel::State::Fullscreen));
    client.draw(b, GREEN);
    client.sync();
    assert!(client.activated(b));
    assert_eq!(compositor.frames().len(), 1);

    client.toplevel(b).unset_fullscreen();
    client.sync();
    assert_eq!(client.size(a), LEFT);
}

#[test]
fn a_new_window_ends_fullscreen() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(ALT, KEY_F);
    client.sync();
    let c = client.open_window(BLUE);
    client.sync();
    assert!(client.activated(c));
    assert_eq!(compositor.frames().len(), 3);
    assert_eq!(client.size(a), LEFT);
    assert!(!client.has_state(b, xdg_toplevel::State::Fullscreen));
}

#[test]
fn closing_a_fullscreen_window_brings_the_others_back() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(ALT, KEY_F);
    client.sync();
    client.close(b);
    client.sync();
    assert_eq!(client.size(a), FULL);
    assert!(client.activated(a));
    assert_eq!(compositor.frames().len(), 1);
}

#[test]
fn focus_next_leaves_fullscreen_instead_of_focusing_a_hidden_window() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(ALT, KEY_F);
    client.sync();
    compositor.chord(SUPER, KEY_TAB);
    client.sync();
    assert!(client.activated(a));
    assert_eq!(compositor.frames().len(), 2);
    assert_eq!(client.size(b), RIGHT);
}

#[test]
fn fullscreen_is_per_workspace() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(ALT, KEY_F);
    client.sync();
    compositor.chord(SUPER, KEY_2);
    assert!(compositor.frames().is_empty());
    let c = client.open_window(BLUE);
    assert_eq!(client.size(c), FULL);
    compositor.chord(SUPER, KEY_1);
    client.sync();
    assert_eq!(compositor.frames().len(), 1);
    assert_eq!(client.size(b), (WIDTH, HEIGHT));
    assert!(client.activated(b));
    let _ = a;
}

#[test]
fn super_left_drag_moves_a_floating_window_and_hides_the_press() {
    let (compositor, mut client, _, _) = floating_dialog();
    super_drag(&compositor, BTN_LEFT, (400.0, 300.0), (500.0, 350.0));
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(450, 250, 300, 200))
    );
    assert!(
        !client
            .take_input()
            .iter()
            .any(|input| matches!(input, Input::Button(..)))
    );
    assert_eq!(compositor.screenshot().at(700, 400), GREEN);
}

#[test]
fn the_pointer_returns_to_the_client_after_a_drag() {
    let (compositor, mut client, _, dialog) = floating_dialog();
    super_drag(&compositor, BTN_LEFT, (400.0, 300.0), (500.0, 350.0));
    assert!(client.take_input().contains(&Input::PointerLeave(dialog)));
    compositor.pointer_at(510.0, 360.0);
    compositor.click();
    assert!(
        client
            .take_input()
            .contains(&Input::Button(BTN_LEFT, wl_pointer::ButtonState::Pressed))
    );
}

#[test]
fn super_right_drag_resizes_from_the_nearest_corner() {
    let (compositor, mut client, _, dialog) = floating_dialog();
    // Bottom right quarter: the top left corner stays.
    super_drag(&compositor, BTN_RIGHT, (600.0, 350.0), (650.0, 400.0));
    client.sync();
    assert_eq!(client.size(dialog), (350, 250));
    client.draw(dialog, GREEN);
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(350, 200, 350, 250))
    );

    // Top left quarter: the bottom right corner stays.
    super_drag(&compositor, BTN_RIGHT, (400.0, 250.0), (380.0, 220.0));
    client.sync();
    assert_eq!(client.size(dialog), (370, 280));
    client.draw(dialog, GREEN);
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(330, 170, 370, 280))
    );
}

#[test]
fn resizing_tells_the_client_while_it_lasts() {
    let (compositor, mut client, _, dialog) = floating_dialog();
    compositor.pointer_at(600.0, 350.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_RIGHT, ButtonState::Pressed);
    client.sync();
    assert!(client.has_state(dialog, xdg_toplevel::State::Resizing));
    compositor.button(BTN_RIGHT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    client.sync();
    assert!(!client.has_state(dialog, xdg_toplevel::State::Resizing));
}

#[test]
fn resizing_stops_at_the_minimum_size() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let main = client.open_window(RED);
    let parent = client.toplevel(main);
    let dialog = client.open_sized_window((300, 200), GREEN, |toplevel| {
        toplevel.set_parent(Some(&parent));
        toplevel.set_min_size(200, 150);
    });
    client.sync();
    super_drag(&compositor, BTN_RIGHT, (600.0, 350.0), (100.0, 50.0));
    client.sync();
    assert_eq!(client.size(dialog), (200, 150));
}

#[test]
fn super_right_drag_on_a_tile_moves_the_divider() {
    let (compositor, mut client, a, b) = two_windows();
    super_drag(&compositor, BTN_RIGHT, (750.0, 300.0), (650.0, 300.0));
    client.sync();
    assert_eq!(client.size(a), (LEFT.0 - 100, LEFT.1));
    assert_eq!(client.size(b), (RIGHT.0 + 100, RIGHT.1));
    assert!(compositor.with(|state| state.drag.is_none()));
}

#[test]
fn dragging_a_tile_onto_another_swaps_them() {
    let (compositor, mut client, a, b) = two_windows();
    super_drag(&compositor, BTN_LEFT, (250.0, 300.0), (750.0, 300.0));
    client.sync();
    assert_eq!(client.size(a), RIGHT);
    assert_eq!(client.size(b), LEFT);
    assert!(client.activated(a));
}

#[test]
fn dropping_a_tile_on_itself_changes_nothing() {
    let (compositor, mut client, a, b) = two_windows();
    super_drag(&compositor, BTN_LEFT, (250.0, 300.0), (260.0, 310.0));
    client.sync();
    assert_eq!(client.size(a), LEFT);
    assert_eq!(client.size(b), RIGHT);
}

#[test]
fn a_fullscreen_window_cannot_be_dragged() {
    let (compositor, mut client, _, b) = two_windows();
    compositor.chord(ALT, KEY_F);
    client.sync();
    super_drag(&compositor, BTN_LEFT, (250.0, 300.0), (750.0, 300.0));
    client.sync();
    assert_eq!(client.size(b), (WIDTH, HEIGHT));
    assert!(compositor.with(|state| state.drag.is_none()));
}

#[test]
fn a_window_closing_during_a_drag_ends_it_quietly() {
    let (compositor, mut client, _, dialog) = floating_dialog();
    compositor.pointer_at(400.0, 300.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    client.close(dialog);
    compositor.pointer_at(500.0, 350.0);
    compositor.button(BTN_LEFT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    assert!(compositor.with(|state| state.drag.is_none()));
    compositor.pointer_at(510.0, 350.0);
    compositor.click();
}

#[test]
fn a_client_can_start_a_move_and_a_resize_by_itself() {
    let (compositor, mut client, _, dialog) = floating_dialog();
    let seat = client.seat();

    // As a title bar would: press, then ask to move.
    compositor.pointer_at(400.0, 300.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    client
        .toplevel(dialog)
        ._move(&seat, client.state.button_serial);
    client.sync();
    assert!(compositor.with(|state| state.drag.is_some()));
    compositor.pointer_at(450.0, 310.0);
    compositor.button(BTN_LEFT, ButtonState::Released);
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(400, 210, 300, 200))
    );

    // As a left border would.
    compositor.pointer_at(405.0, 300.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    client.toplevel(dialog).resize(
        &seat,
        client.state.button_serial,
        xdg_toplevel::ResizeEdge::Left,
    );
    client.sync();
    compositor.pointer_at(375.0, 300.0);
    compositor.button(BTN_LEFT, ButtonState::Released);
    client.sync();
    assert_eq!(client.size(dialog), (330, 200));
    client.draw(dialog, GREEN);
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(370, 210, 330, 200))
    );
}

#[test]
fn a_move_request_with_a_stale_serial_is_ignored() {
    let (compositor, mut client, _, dialog) = floating_dialog();
    let seat = client.seat();
    client.toplevel(dialog)._move(&seat, 1);
    client.sync();
    assert!(compositor.with(|state| state.drag.is_none()));
}

#[test]
fn sloppy_focus_follows_the_pointer_between_windows() {
    let (compositor, mut client, a, b) = mango_like();
    assert!(client.activated(b));
    compositor.pointer_at(250.0, 300.0);
    client.sync();
    assert!(client.activated(a));
    compositor.pointer_at(750.0, 300.0);
    client.sync();
    assert!(client.activated(b));
}

#[test]
fn sloppy_focus_keeps_focus_on_the_background() {
    let (compositor, mut client, _, b) = mango_like();
    compositor.pointer_at(2.0, 2.0);
    client.sync();
    assert!(client.activated(b));
}

#[test]
fn sloppy_focus_needs_motion_and_not_just_a_window_appearing() {
    let (compositor, mut client, a, _) = mango_like();
    compositor.pointer_at(250.0, 300.0);
    client.sync();
    assert!(client.activated(a));
    // Opening a window focuses it, and the pointer resting elsewhere does not take that back.
    let c = client.open_window(BLUE);
    client.sync();
    assert!(client.activated(c));
    compositor.with(|state| state.retile());
    client.sync();
    assert!(client.activated(c));
}

#[test]
fn sloppy_focus_does_not_raise_but_a_click_does() {
    let compositor = Compositor::start_with("");
    let mut client = Client::connect(&compositor);
    let main = client.open_window(RED);
    let parent = client.toplevel(main);
    let first = client.open_sized_window((300, 300), GREEN, |toplevel| {
        toplevel.set_parent(Some(&parent));
    });
    let second = client.open_sized_window((300, 300), BLUE, |toplevel| {
        toplevel.set_parent(Some(&parent));
    });
    // Same size and centre: only move the second one aside to see both.
    compositor.with(|state| {
        let workspace = state.monitors[0].workspace_mut();
        workspace.floating[1].rect.x += 150;
        state.retile();
    });
    let shot = compositor.screenshot();
    assert_eq!(shot.at(500, 300), BLUE);

    // The left part of the lower window is uncovered. Hovering focuses, but keeps the order.
    compositor.pointer_at(400.0, 300.0);
    client.sync();
    assert!(client.activated(first));
    assert_eq!(compositor.screenshot().at(500, 300), BLUE);

    compositor.click();
    client.sync();
    assert_eq!(compositor.screenshot().at(500, 300), GREEN);
    let _ = second;
}

#[test]
fn a_held_button_stops_sloppy_focus() {
    let (compositor, mut client, a, b) = mango_like();
    compositor.pointer_at(250.0, 300.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    compositor.pointer_at(750.0, 300.0);
    client.sync();
    assert!(client.activated(a), "a text selection must not lose focus");
    compositor.button(BTN_LEFT, ButtonState::Released);
    compositor.pointer_at(751.0, 300.0);
    client.sync();
    assert!(client.activated(b));
}

#[test]
fn keyboard_focus_changes_warp_the_pointer_like_mango() {
    let (compositor, mut client, a, _) = mango_like();
    compositor.pointer_at(750.0, 300.0);
    compositor.chord(ALT, KEY_LEFT);
    client.sync();
    assert!(client.activated(a));
    let location = compositor.with(|state| {
        let pointer = state.seat.get_pointer().unwrap();
        pointer.current_location()
    });
    assert_eq!((location.x, location.y), (254.0, 300.0));
    assert!(
        client
            .take_input()
            .contains(&Input::PointerEnter(a, 240.0, 286.0))
    );
}

#[test]
fn warping_can_be_turned_off() {
    let compositor = Compositor::start_with("warp_cursor = 0\n");
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_window(GREEN);
    compositor.pointer_at(750.0, 300.0);
    compositor.chord(ALT, KEY_LEFT);
    let location = compositor.with(|state| {
        let pointer = state.seat.get_pointer().unwrap();
        pointer.current_location()
    });
    assert_eq!((location.x, location.y), (750.0, 300.0));
}

#[test]
fn each_output_tiles_its_own_windows() {
    let (compositor, _second) = two_outputs();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);

    compositor.pointer_at(1500.0, 300.0);
    let c = client.open_window(BLUE);
    client.sync();
    assert_eq!(client.size(c), FULL);
    assert_eq!(client.size(a), LEFT);
    assert_eq!(client.size(b), RIGHT);
    assert_eq!(
        compositor.with(|state| state.monitors[1].workspace().windows().len()),
        1
    );
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(1014, 14, 972, 572))
    );

    assert_eq!(compositor.screenshot_of(0).at(250, 300), RED);
    assert_eq!(compositor.screenshot_of(1).at(500, 300), BLUE);
    assert_eq!(
        compositor.screenshot_of(1).at(2, 2),
        rgb(Config::default().background_color)
    );
}

#[test]
fn workspaces_are_per_output() {
    let (compositor, _second) = two_outputs();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    compositor.pointer_at(1500.0, 300.0);
    let b = client.open_window(GREEN);

    // Output 2 goes to workspace 2, output 1 stays where it is.
    compositor.chord(SUPER, KEY_2);
    client.sync();
    assert_eq!(compositor.frames(), [Rect::new(14, 14, 972, 572)]);
    assert_eq!(compositor.with(|state| state.monitors[0].active), 0);
    assert_eq!(compositor.with(|state| state.monitors[1].active), 1);
    assert_eq!(compositor.focused_rect(), None);

    compositor.chord(SUPER, KEY_1);
    client.sync();
    assert!(client.activated(b));
    assert!(!client.activated(a));
}

#[test]
fn popups_stay_on_the_output_of_their_window() {
    let (compositor, _second) = two_outputs();
    let mut client = Client::connect(&compositor);
    compositor.pointer_at(1500.0, 300.0);
    let a = client.open_window(RED);
    let _popup = client.open_edge_popup(a);
    // Measured against the first output it would have room to stay at 961.
    assert_eq!(client.state.popup_position, Some((886, 21)));
}

#[test]
fn focus_and_windows_move_between_outputs() {
    let (compositor, _second) = two_outputs();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);

    let alt_shift = &[KEY_LEFTALT, KEY_LEFTSHIFT];
    compositor.chord(alt_shift, KEY_RIGHT);
    client.sync();
    assert_eq!(compositor.with(|state| state.active_monitor), 1);
    assert_eq!(compositor.focused_rect(), None);
    let location = compositor.with(|state| state.seat.get_pointer().unwrap().current_location());
    assert_eq!((location.x, location.y), (1500.0, 300.0));
    compositor.chord(alt_shift, KEY_LEFT);
    client.sync();
    assert_eq!(compositor.with(|state| state.active_monitor), 0);
    assert!(client.activated(b));

    compositor.chord(&[KEY_LEFTMETA, KEY_LEFTALT], KEY_RIGHT);
    client.sync();
    assert_eq!(client.size(a), FULL);
    assert_eq!(client.size(b), FULL);
    assert!(client.activated(b));
    assert_eq!(compositor.with(|state| state.active_monitor), 1);
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(1014, 14, 972, 572))
    );

    // Nothing lies further right.
    compositor.chord(&[KEY_LEFTMETA, KEY_LEFTALT], KEY_RIGHT);
    assert_eq!(compositor.with(|state| state.active_monitor), 1);
}

#[test]
fn a_floating_window_dragged_across_outputs_changes_workspace() {
    let (compositor, _second) = two_outputs();
    let mut client = Client::connect(&compositor);
    let main = client.open_window(RED);
    let parent = client.toplevel(main);
    let _dialog = client.open_sized_window((300, 200), GREEN, |toplevel| {
        toplevel.set_parent(Some(&parent));
    });
    client.sync();

    super_drag(&compositor, BTN_LEFT, (400.0, 300.0), (1400.0, 300.0));
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(1350, 200, 300, 200))
    );
    assert_eq!(
        compositor.with(|state| state.monitors[1].workspace().floating.len()),
        1
    );
    assert_eq!(
        compositor.with(|state| state.monitors[0].workspace().floating.len()),
        0
    );
    assert_eq!(compositor.screenshot_of(1).at(500, 300), GREEN);
}

#[test]
fn a_tile_dropped_on_another_output_joins_its_tree() {
    let (compositor, _second) = two_outputs();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);
    super_drag(&compositor, BTN_LEFT, (750.0, 300.0), (1500.0, 300.0));
    client.sync();
    assert_eq!(client.size(a), FULL);
    assert_eq!(client.size(b), FULL);
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(1014, 14, 972, 572))
    );
}

#[test]
fn an_unplugged_output_hands_over_its_windows() {
    let (compositor, second) = two_outputs();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    compositor.pointer_at(1500.0, 300.0);
    let b = client.open_window(GREEN);
    compositor.chord(SUPER_SHIFT, KEY_2);
    client.sync();
    let c = client.open_window(BLUE);
    client.sync();

    compositor.with(move |state| state.remove_output(&second));
    client.sync();
    assert_eq!(compositor.with(|state| state.monitors.len()), 1);
    assert_eq!(compositor.with(|state| state.active_monitor), 0);
    assert_eq!(client.size(a), FULL);
    assert!(
        client.activated(a),
        "the focused window went to a hidden workspace"
    );
    assert!(!client.activated(b));
    assert!(!client.activated(c));
    // The pointer was left on the vanished output.
    let location = compositor.with(|state| state.seat.get_pointer().unwrap().current_location());
    assert_eq!((location.x, location.y), (500.0, 300.0));

    compositor.chord(SUPER, KEY_2);
    client.sync();
    assert_eq!(compositor.frames().len(), 2);
}

#[test]
fn windows_survive_the_last_output_going_away() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);
    let output = compositor.with(|state| state.monitors[0].output.clone());
    compositor.with(move |state| state.remove_output(&output));
    client.sync();
    assert_eq!(compositor.frames().len(), 0);

    // A commit in this state must not crash, and new windows wait for an output.
    client.draw(a, RED);
    assert_eq!(compositor.with(|state| state.frames.len()), 0);

    compositor.add_output("new", (0, 0));
    client.sync();
    assert_eq!(compositor.frames().len(), 2);
    assert_eq!(client.size(a), LEFT);
    assert_eq!(client.size(b), RIGHT);
    assert!(client.activated(b));
    assert_eq!(compositor.screenshot().at(750, 300), GREEN);
}

#[test]
fn a_shrinking_output_pulls_floating_windows_back_in() {
    let (compositor, mut client, _, _) = floating_dialog();
    super_drag(&compositor, BTN_LEFT, (400.0, 300.0), (900.0, 500.0));
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        let mode = Mode {
            size: (600, 400).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, None);
        state.output_resized(&output);
    });
    client.sync();
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(300, 200, 300, 200))
    );
}

#[test]
fn surface_coordinates_are_relative_on_the_second_output() {
    let (compositor, _second) = two_outputs();
    let mut client = Client::connect(&compositor);
    compositor.pointer_at(1500.0, 300.0);
    client.open_window(RED);
    client.take_input();
    compositor.pointer_at(1600.0, 200.0);
    let input = client.take_input();
    assert!(
        input.contains(&Input::PointerMotion(586.0, 186.0)),
        "{input:?}"
    );
}

#[test]
fn a_wheel_scrolls_with_value_and_notches_on_both_axes() {
    let (compositor, mut client, _, _) = two_windows();
    compositor.pointer_at(250.0, 300.0);
    client.take_input();

    scroll(
        &compositor,
        |scroll| scroll.set(Axis::Vertical, None, Some(120.0)),
        AxisSource::Wheel,
    );
    assert_eq!(
        client.take_input(),
        [
            Input::AxisSource(wl_pointer::AxisSource::Wheel),
            Input::AxisV120(wl_pointer::Axis::VerticalScroll, 120),
            Input::Axis(wl_pointer::Axis::VerticalScroll, 15.0),
        ]
    );

    scroll(
        &compositor,
        |scroll| scroll.set(Axis::Horizontal, None, Some(-60.0)),
        AxisSource::Wheel,
    );
    assert_eq!(
        client.take_input(),
        [
            Input::AxisSource(wl_pointer::AxisSource::Wheel),
            Input::AxisV120(wl_pointer::Axis::HorizontalScroll, -60),
            Input::Axis(wl_pointer::Axis::HorizontalScroll, -7.5),
        ]
    );
}

#[test]
fn a_touchpad_scrolls_smoothly_and_stops() {
    let (compositor, mut client, _, _) = two_windows();
    compositor.pointer_at(250.0, 300.0);
    client.take_input();

    scroll(
        &compositor,
        |scroll| {
            scroll.set(Axis::Horizontal, Some(1.5), None);
            scroll.set(Axis::Vertical, Some(-2.5), None);
        },
        AxisSource::Finger,
    );
    let input = client.take_input();
    assert!(input.contains(&Input::AxisSource(wl_pointer::AxisSource::Finger)));
    assert!(input.contains(&Input::Axis(wl_pointer::Axis::HorizontalScroll, 1.5)));
    assert!(input.contains(&Input::Axis(wl_pointer::Axis::VerticalScroll, -2.5)));

    scroll(
        &compositor,
        |scroll| {
            scroll.set(Axis::Horizontal, Some(0.0), None);
            scroll.set(Axis::Vertical, Some(0.0), None);
        },
        AxisSource::Finger,
    );
    let input = client.take_input();
    assert!(input.contains(&Input::AxisStop(wl_pointer::Axis::HorizontalScroll)));
    assert!(input.contains(&Input::AxisStop(wl_pointer::Axis::VerticalScroll)));
}

#[test]
fn scrolling_reaches_the_window_under_the_pointer_only() {
    let (compositor, mut client, _, _) = two_windows();
    compositor.pointer_at(2.0, 2.0);
    client.take_input();
    scroll(
        &compositor,
        |scroll| scroll.set(Axis::Vertical, None, Some(120.0)),
        AxisSource::Wheel,
    );
    assert_eq!(client.take_input(), []);
}

#[test]
fn stacking_order_survives_a_trip_through_a_hidden_workspace() {
    let (compositor, _client, _, _) = floating_dialog();
    compositor.chord(SUPER, KEY_2);
    compositor.chord(SUPER, KEY_1);
    assert_eq!(compositor.screenshot().at(500, 300), GREEN);
    let placements = compositor.with(|state| {
        let workspace = state.monitors[0].workspace();
        workspace
            .windows()
            .iter()
            .map(|window| workspace.placement(window))
            .collect::<Vec<_>>()
    });
    assert_eq!(placements[0], Some(Placement::Tiled));
    assert!(matches!(placements[1], Some(Placement::Floating(_))));
}

#[test]
fn pos_prints_the_exact_position_and_output_once() {
    let compositor = Compositor::start();
    compositor.pointer_at(123.25, 45.0625);
    assert_eq!(compositor.run(&["pos", "--once"]), "123.25 45.0625 test\n");
    assert_eq!(
        compositor.run(&["pos", "--once", "--json"]),
        "{\"x\":123.25,\"y\":45.0625,\"output\":\"test\"}\n"
    );
}

#[test]
fn pos_now_prints_once_like_once() {
    let compositor = Compositor::start();
    compositor.pointer_at(123.25, 45.0625);
    assert_eq!(compositor.run(&["pos", "now"]), "123.25 45.0625 test\n");
    assert_eq!(compositor.run(&["pos", "0"]), "123.25 45.0625 test\n");
    assert_eq!(
        compositor.run(&["pos", "now", "--json"]),
        "{\"x\":123.25,\"y\":45.0625,\"output\":\"test\"}\n"
    );
}

#[test]
fn pos_once_takes_the_axis_and_decimals_like_loop() {
    let compositor = Compositor::start();
    compositor.pointer_at(123.25, 45.5);
    assert_eq!(compositor.run(&["pos", "now", "x", "0"]), "123\n");
    assert_eq!(compositor.run(&["pos", "now", "y"]), "45.5\n");
    assert_eq!(compositor.run(&["pos", "now", "xy", "1"]), "123.2 45.5\n");
    assert_eq!(compositor.run(&["pos", "now", "max"]), "123.25 45.5 test\n");
    assert_eq!(compositor.run(&["pos", "now", "2"]), "123.25 45.50 test\n");
    assert_eq!(compositor.run(&["pos", "0", "xy", "0"]), "123 46\n");
    assert_eq!(compositor.run(&["pos", "1", "y", "2"]), "45.50\n");
}

#[test]
fn pos_with_a_delay_prints_where_the_pointer_is_after_it() {
    const DELAY: Duration = Duration::from_millis(1500);
    let compositor = Compositor::start();
    compositor.pointer_at(1.0, 2.0);
    let start = Instant::now();
    let child = compositor
        .cli(&["pos", &DELAY.as_millis().to_string()])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    compositor.pointer_at(30.5, 40.0);
    let output = child.wait_with_output().unwrap();
    let elapsed = start.elapsed();
    assert!(elapsed >= DELAY, "printed after {elapsed:?}");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "30.5 40.0 test\n"
    );
}

#[test]
fn pos_loop_follows_like_pos() {
    let compositor = Compositor::start();
    compositor.pointer_at(10.0, 20.0);
    let watcher = Watcher::start(&compositor, &["loop"]);
    assert_eq!(watcher.next(), "10.0 20.0 test");
    compositor.pointer_at(11.5, 20.0);
    assert_eq!(watcher.next(), "11.5 20.0 test");
}

#[test]
fn pos_loop_shows_one_axis_and_only_its_changes() {
    let compositor = Compositor::start();
    compositor.pointer_at(10.5, 20.25);
    let x = Watcher::start(&compositor, &["loop", "x"]);
    let y = Watcher::start(&compositor, &["loop", "y"]);
    let xy = Watcher::start(&compositor, &["loop", "xy", "max"]);
    assert_eq!(x.next(), "10.5");
    assert_eq!(y.next(), "20.25");
    assert_eq!(xy.next(), "10.5 20.25");

    // Moving along one axis shows nothing on the other.
    compositor.pointer_at(10.5, 30.0);
    assert_eq!(y.next(), "30.0");
    assert_eq!(xy.next(), "10.5 30.0");
    compositor.pointer_at(11.5, 30.0);
    assert_eq!(x.next(), "11.5");
    assert_eq!(xy.next(), "11.5 30.0");
}

#[test]
fn pos_loop_rounds_to_the_decimals_and_skips_what_does_not_show() {
    let compositor = Compositor::start();
    compositor.pointer_at(10.125, 20.75);
    let none = Watcher::start(&compositor, &["loop", "xy", "0"]);
    let two = Watcher::start(&compositor, &["loop", "x", "2"]);
    let with_output = Watcher::start(&compositor, &["loop", "1"]);
    assert_eq!(none.next(), "10 21");
    assert_eq!(two.next(), "10.12");
    assert_eq!(with_output.next(), "10.1 20.8 test");

    compositor.pointer_at(10.25, 20.75);
    assert_eq!(two.next(), "10.25");
    assert_eq!(with_output.next(), "10.2 20.8 test");
    compositor.pointer_at(12.0, 20.75);
    assert_eq!(none.next(), "12 21");
}

#[test]
fn pos_shows_a_change_at_once_and_only_a_change() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    compositor.pointer_at(10.0, 20.0);
    let watcher = Watcher::start(&compositor, &[]);
    assert_eq!(watcher.next(), "10.0 20.0 test");

    compositor.pointer_at(11.5, 20.0);
    assert_eq!(watcher.next(), "11.5 20.0 test");
    // Things that re-evaluate the pointer without moving it print nothing.
    compositor.pointer_at(11.5, 20.0);
    client.open_window(GREEN);
    client.close(0);
    compositor.with(Wirbel::refresh_pointer_focus);
    compositor.run(&["move", "11.5", "20"]);
    compositor.run(&["move", "--rel", "0", "0"]);
    compositor.pointer_at(12.0, 20.0);
    assert_eq!(watcher.next(), "12.0 20.0 test");
}

#[test]
fn pos_follows_every_way_the_pointer_can_move() {
    let (compositor, mut client, a, _) = mango_like();
    let second = compositor.add_output("second", (WIDTH, 0));
    client.sync();
    compositor.pointer_at(750.0, 300.0);
    let watcher = Watcher::start(&compositor, &[]);
    assert_eq!(watcher.next(), "750.0 300.0 test");

    // A real device.
    compositor.pointer_at(751.0, 300.0);
    assert_eq!(watcher.next(), "751.0 300.0 test");
    // `wirbel move`, absolute and relative, also from another connection.
    compositor.run(&["move", "1500", "300.5"]);
    assert_eq!(watcher.next(), "1500.0 300.5 second");
    compositor.run(&["move", "--rel", "-750", "-0.5"]);
    assert_eq!(watcher.next(), "750.0 300.0 test");
    // A warp after a change of keyboard focus.
    compositor.chord(ALT, KEY_LEFT);
    client.sync();
    assert!(client.activated(a));
    assert_eq!(watcher.next(), "254.0 300.0 test");
    // A warp to an empty output.
    compositor.chord(&[KEY_LEFTALT, KEY_LEFTSHIFT], KEY_RIGHT);
    assert_eq!(watcher.next(), "1500.0 300.0 second");
    // The output under the pointer going away moves it, possibly in two steps.
    compositor.with(move |state| state.remove_output(&second));
    let mut line = watcher.next();
    while line != "500.0 300.0 test" {
        assert!(line.ends_with(" test"), "{line}");
        line = watcher.next();
    }
}

#[test]
fn pos_notices_outputs_changing_under_a_resting_pointer() {
    let compositor = Compositor::start();
    compositor.pointer_at(999.0, 300.0);
    let watcher = Watcher::start(&compositor, &[]);
    assert_eq!(watcher.next(), "999.0 300.0 test");

    // Resizing the output away from under the pointer drags it along.
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        let mode = Mode {
            size: (800, 400).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, None);
        state.output_resized(&output);
    });
    assert_eq!(watcher.next(), "799.0 300.0 test");

    // The position stays, but it is on no output, and then on a new one.
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        state.remove_output(&output);
    });
    assert_eq!(watcher.next(), "799.0 300.0 -");
    compositor.add_output("new", (0, 0));
    assert_eq!(watcher.next(), "799.0 300.0 new");
}

#[test]
fn pos_as_json_streams_one_object_per_line() {
    let compositor = Compositor::start();
    compositor.pointer_at(1.0, 2.0);
    let watcher = Watcher::start(&compositor, &["--json"]);
    assert_eq!(watcher.next(), r#"{"x":1.0,"y":2.0,"output":"test"}"#);
    compositor.pointer_at(1.5, 2.0);
    assert_eq!(watcher.next(), r#"{"x":1.5,"y":2.0,"output":"test"}"#);
}

#[test]
fn watchers_are_independent_of_each_other() {
    let compositor = Compositor::start();
    let first = Watcher::start(&compositor, &[]);
    first.next();
    let second = Watcher::start(&compositor, &[]);
    second.next();
    drop(first);
    compositor.pointer_at(5.0, 5.0);
    assert_eq!(second.next(), "5.0 5.0 test");
}

#[test]
fn pos_ends_quietly_when_its_reader_goes_away() {
    let compositor = Compositor::start();
    let mut child = compositor
        .cli(&["pos"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    stdout.read_line(&mut String::new()).unwrap();
    drop(stdout);
    compositor.pointer_at(5.0, 5.0);
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < ANSWER, "pos kept running");
        compositor.pointer_at(start.elapsed().as_secs_f64(), 5.0);
    }
    assert_eq!(child.wait().unwrap().code(), Some(0));
}

#[test]
fn pos_reports_the_end_of_the_compositor() {
    let compositor = Compositor::start();
    let mut watcher = Watcher::start(&compositor, &[]);
    watcher.next();
    drop(compositor);
    assert_eq!(watcher.child.wait().unwrap().code(), Some(1));
}

#[test]
fn commands_explain_a_missing_compositor() {
    let output = Command::new(env!("CARGO_BIN_EXE_wirbel"))
        .args(["move", "1", "2"])
        .env("WAYLAND_DISPLAY", "wayland-nobody")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.starts_with("wirbel move: no wirbel on wayland-nobody"),
        "{stderr}"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_wirbel"))
        .args(["pos", "--once"])
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("WAYLAND_DISPLAY"));
}

#[test]
fn click_clicks_where_the_pointer_is() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    let b = client.open_window(0x0000_ff00);
    compositor.pointer_at(100.0, 100.0);
    client.take_input();
    assert!(client.activated(b));

    assert_eq!(compositor.run(&["click"]), "");
    client.sync();
    assert_eq!(
        client.take_input(),
        [
            Input::KeyboardLeave(b),
            Input::KeyboardEnter(a),
            Input::Button(BTN_LEFT, wl_pointer::ButtonState::Pressed),
            Input::Button(BTN_LEFT, wl_pointer::ButtonState::Released),
        ]
    );
}

#[test]
fn click_repeats_with_the_given_gap() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(0x00ff_0000);
    compositor.pointer_at(100.0, 100.0);
    client.take_input();

    let start = Instant::now();
    compositor.run(&["click", "3", "0.25"]);
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_millis(500), "took {elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    client.sync();
    assert_eq!(presses(&client.take_input()), 3);

    compositor.run(&["click", "2", "0"]);
    client.sync();
    assert_eq!(presses(&client.take_input()), 2);
}

#[test]
fn click_loop_goes_on_until_stopped() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(0x00ff_0000);
    compositor.pointer_at(100.0, 100.0);
    client.take_input();

    let mut child = compositor.cli(&["click", "loop", "0.05"]).spawn().unwrap();
    let start = Instant::now();
    thread::sleep(Duration::from_millis(500));
    assert_eq!(
        child.try_wait().unwrap(),
        None,
        "after {:?}",
        start.elapsed()
    );
    child.kill().unwrap();
    child.wait().unwrap();
    thread::sleep(Duration::from_millis(100));
    client.sync();
    assert!(presses(&client.take_input()) >= 3);

    // Whatever click was on its way, nothing stays down.
    assert!(!compositor.with(|state| state.left_held));
}

#[test]
fn click_leaves_a_held_button_alone() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(0x00ff_0000);
    compositor.pointer_at(100.0, 100.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    client.take_input();

    assert!(compositor.run_failing(&["click"]).contains("held down"));
    client.sync();
    assert_eq!(client.take_input(), []);
    assert!(compositor.with(|state| state.left_held));
    compositor.button(BTN_LEFT, ButtonState::Released);
}

#[test]
fn a_click_does_not_release_twice_if_the_hand_was_faster() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(0x00ff_0000);
    compositor.pointer_at(100.0, 100.0);
    client.take_input();

    let mut ipc = compositor.raw_ipc();
    ipc.send("click\n");
    while !compositor.with(|state| state.left_held) {
        thread::yield_now();
    }
    compositor.button(BTN_LEFT, ButtonState::Released);
    assert_eq!(ipc.line(), "ok");
    client.sync();
    let released = Input::Button(BTN_LEFT, wl_pointer::ButtonState::Released);
    let input = client.take_input();
    assert_eq!(input.iter().filter(|event| **event == released).count(), 1);
}

#[test]
fn distance_adds_up_the_way_over_the_time() {
    let compositor = Compositor::start();
    compositor.pointer_at(10.0, 10.0);
    let child = compositor
        .cli(&["distance", "1", "2"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    compositor.pointer_at(13.0, 14.0);
    compositor.pointer_at(10.0, 10.0);
    compositor.pointer_at(10.0, 10.5);
    let start = Instant::now();
    let output = child.wait_with_output().unwrap();
    assert!(start.elapsed() < Duration::from_millis(900));
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "10.50\n");
}

#[test]
fn distance_of_a_pointer_at_rest_is_zero() {
    let compositor = Compositor::start();
    compositor.pointer_at(10.0, 10.0);
    assert_eq!(compositor.run(&["distance", "0.1"]), "0.0\n");
    assert_eq!(compositor.run(&["distance", "0", "3"]), "0.000\n");
    assert_eq!(compositor.run(&["distance", "0", "0"]), "0\n");
    assert_eq!(compositor.run(&["distance", "0", "max"]), "0.0\n");
}

#[test]
fn distance_loop_keeps_a_running_total() {
    let compositor = Compositor::start();
    compositor.pointer_at(0.0, 0.0);
    let watcher = Watcher::start_command(&compositor, &["distance", "loop", "1"]);
    assert_eq!(watcher.next(), "0.0");
    compositor.pointer_at(3.0, 4.0);
    assert_eq!(watcher.next(), "5.0");
    compositor.pointer_at(3.0, 5.5);
    assert_eq!(watcher.next(), "6.5");
    // Too little to show.
    compositor.pointer_at(3.0, 5.51);
    compositor.pointer_at(3.0, 7.5);
    assert_eq!(watcher.next(), "8.5");

    let bare = Watcher::start_command(&compositor, &["distance"]);
    assert_eq!(bare.next(), "0.0");
    compositor.pointer_at(3.0, 8.5);
    assert_eq!(bare.next(), "1.0");
}

#[test]
fn move_with_a_time_goes_there_on_a_straight_line() {
    let compositor = Compositor::start();
    compositor.pointer_at(100.0, 100.0);
    let start = Instant::now();
    let mut child = compositor
        .cli(&["move", "500", "300", "0.6"])
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    let (x, y) = compositor.pointer();
    assert!(x > 150.0 && x < 450.0, "at {x} {y}");
    // On the line from the start to the target.
    assert!((y - (100.0 + (x - 100.0) / 2.0)).abs() < 1e-6, "at {x} {y}");
    assert!(child.wait().unwrap().success());
    assert!(start.elapsed() >= Duration::from_millis(600));
    assert_eq!(compositor.pointer(), (500.0, 300.0));

    compositor.run(&["move", "--rel", "-50", "20", "0.1"]);
    assert_eq!(compositor.pointer(), (450.0, 320.0));
    compositor.run(&["move", "7", "8", "0"]);
    assert_eq!(compositor.pointer(), (7.0, 8.0));
}

#[test]
fn ws_switches_the_workspace() {
    let compositor = Compositor::start();
    let active = |compositor: &Compositor| compositor.with(|state| state.monitors[0].active);
    assert_eq!(compositor.run(&["ws", "3"]), "");
    assert_eq!(active(&compositor), 2);
    compositor.run(&["ws", "9"]);
    assert_eq!(active(&compositor), 8);
    compositor.run(&["ws", "1"]);
    assert_eq!(active(&compositor), 0);

    compositor.run(&["ws", "next"]);
    assert_eq!(active(&compositor), 1);
    compositor.run(&["ws", "prev"]);
    compositor.run(&["ws", "prev"]);
    assert_eq!(active(&compositor), 8);
    compositor.run(&["ws", "next"]);
    assert_eq!(active(&compositor), 0);
}

#[test]
fn ws_next_used_skips_empty_workspaces() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let active = |compositor: &Compositor| compositor.with(|state| state.monitors[0].active);
    client.open_window(0x00ff_0000);
    compositor.run(&["ws", "5"]);
    client.open_window(0x0000_ff00);
    compositor.run(&["ws", "3"]);

    compositor.run(&["ws", "next_used"]);
    assert_eq!(active(&compositor), 4);
    // Nothing further on: it stays.
    compositor.run(&["ws", "next_used"]);
    assert_eq!(active(&compositor), 4);
    compositor.run(&["ws", "prev_used"]);
    assert_eq!(active(&compositor), 0);
}

#[test]
fn bad_command_lines_are_refused() {
    let compositor = Compositor::start();
    compositor.pointer_at(7.0, 8.0);
    for args in [
        &["ws"][..],
        &["ws", "0"],
        &["ws", "10"],
        &["ws", "last"],
        &["ws", "next", "2"],
        &["ws", "1", "2"],
        &["click", "0"],
        &["click", "-1"],
        &["click", "two"],
        &["click", "1.5"],
        &["click", "2", "soon"],
        &["click", "2", "-1"],
        &["click", "2", "NaN"],
        &["click", "2", "inf"],
        &["click", "2", "1", "1"],
        &["distance", "soon"],
        &["distance", "-1"],
        &["distance", "1", "18"],
        &["distance", "1", "x"],
        &["distance", "loop", "-1"],
        &["distance", "1", "2", "3"],
        &["pos", "soon"],
        &["pos", "-5"],
        &["pos", "1.5"],
        &["pos", "now", "100"],
        &["pos", "x"],
        &["pos", "max"],
        &["pos", "now", "loop"],
        &["pos", "100", "loop"],
        &["pos", "loop", "now"],
        &["pos", "loop", "yx"],
        &["pos", "loop", "x", "y"],
        &["pos", "loop", "2", "x"],
        &["pos", "loop", "max", "2"],
        &["pos", "loop", "18"],
        &["pos", "loop", "-1"],
        &["pos", "loop", "--once"],
        &["pos", "loop", "x", "--json"],
        &["pos", "now", "18"],
        &["pos", "now", "2", "x"],
        &["pos", "now", "x", "y"],
        &["pos", "now", "x", "--json"],
        &["pos", "100", "1", "x"],
        &["move"],
        &["move", "1"],
        &["move", "1", "2", "3", "4"],
        &["move", "1", "2", "-1"],
        &["move", "1", "2", "soon"],
        &["move", "--rel", "1", "2", "inf"],
        &["move", "a", "2"],
        &["move", "--rel", "1"],
        &["move", "1", "--rel", "2"],
        &["move", "1", "2", "--rel"],
        &["pos", "--nope"],
        &["pos", "extra"],
    ] {
        assert!(compositor.run_failing(args).contains("usage:"), "{args:?}");
    }
    for args in [&["move", "NaN", "2"][..], &["move", "--rel", "1", "inf"]] {
        assert!(compositor.run_failing(args).contains("finite"), "{args:?}");
    }
    assert_eq!(compositor.pointer(), (7.0, 8.0));
}

#[test]
fn move_reaches_clients_as_ordinary_motion() {
    let (compositor, mut client, a, b) = two_windows();
    let rect_a = compositor.frames()[0];
    let rect_b = compositor.frames()[1];
    compositor.run(&["move", "100.5", "200.25"]);
    client.sync();
    assert_eq!(compositor.pointer(), (100.5, 200.25));
    let (x, y) = (100.5 - f64::from(rect_a.x), 200.25 - f64::from(rect_a.y));
    assert_eq!(client.take_input(), [Input::PointerEnter(a, x, y)]);

    compositor.run(&["move", "--rel", "1", "0"]);
    client.sync();
    assert_eq!(client.take_input(), [Input::PointerMotion(x + 1.0, y)]);

    compositor.run(&["move", "800", "200"]);
    client.sync();
    let (x, y) = (800.0 - f64::from(rect_b.x), 200.0 - f64::from(rect_b.y));
    assert_eq!(
        client.take_input(),
        [Input::PointerLeave(a), Input::PointerEnter(b, x, y)]
    );

    // And a click lands where the pointer went.
    compositor.click();
    client.sync();
    assert!(
        client
            .take_input()
            .contains(&Input::Button(BTN_LEFT, wl_pointer::ButtonState::Pressed))
    );
}

#[test]
fn move_focuses_like_a_mouse_under_sloppy_focus() {
    let (compositor, mut client, a, b) = mango_like();
    compositor.pointer_at(750.0, 300.0);
    client.sync();
    assert!(client.activated(b));
    compositor.run(&["move", "100", "300"]);
    client.sync();
    assert!(client.activated(a));
    assert!(!client.activated(b));
}

#[test]
fn move_is_clamped_to_the_output() {
    let compositor = Compositor::start();
    for (target, expected) in [
        (["move", "-5", "-5"], (0.0, 0.0)),
        (["move", "1000", "600"], (999.0, 599.0)),
        (["move", "1e300", "100.5"], (999.0, 100.5)),
        (["move", "100.5", "-1e300"], (100.5, 0.0)),
        (["move", "999.75", "599.5"], (999.75, 599.5)),
    ] {
        compositor.run(&target);
        assert_eq!(compositor.pointer(), expected, "{target:?}");
    }

    compositor.run(&["move", "10", "10"]);
    compositor.run(&["move", "--rel", "-100", "-100"]);
    assert_eq!(compositor.pointer(), (0.0, 0.0));
    compositor.run(&["move", "--rel", "5000", "50"]);
    assert_eq!(compositor.pointer(), (999.0, 50.0));
}

#[test]
fn move_crosses_outputs_and_stays_on_the_visible_area() {
    // Two outputs of the same size, the second one lower: below the first is nothing to be on.
    let compositor = Compositor::start();
    compositor.add_output("lower", (WIDTH, 300));

    compositor.run(&["move", "1500", "500"]);
    assert_eq!(compositor.pointer(), (1500.0, 500.0));
    assert_eq!(compositor.with(|state| state.active_monitor), 1);
    assert_eq!(compositor.run(&["pos", "--once"]), "1500.0 500.0 lower\n");

    // Into the corners that belong to no output.
    compositor.run(&["move", "1500", "100"]);
    assert_eq!(compositor.pointer(), (1500.0, 300.0));
    compositor.run(&["move", "500", "800"]);
    assert_eq!(compositor.pointer(), (500.0, 599.0));
    assert_eq!(compositor.with(|state| state.active_monitor), 0);
    // Relative across the seam, and past the far end.
    compositor.run(&["move", "999", "400"]);
    compositor.run(&["move", "--rel", "1.5", "0"]);
    assert_eq!(compositor.pointer(), (1000.5, 400.0));
    assert_eq!(compositor.with(|state| state.active_monitor), 1);
    compositor.run(&["move", "--rel", "5000", "0"]);
    assert_eq!(compositor.pointer(), (1999.0, 400.0));
}

#[test]
fn device_motion_is_clamped_like_move() {
    let compositor = Compositor::start();
    compositor.pointer_at(-50.0, 2000.0);
    assert_eq!(compositor.pointer(), (0.0, 599.0));
}

#[test]
fn move_during_a_drag_carries_the_window_along() {
    let (compositor, mut client, _, dialog) = floating_dialog();
    compositor.pointer_at(400.0, 300.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    let watcher = Watcher::start(&compositor, &[]);
    assert_eq!(watcher.next(), "400.0 300.0 test");
    client.sync();
    client.take_input();

    compositor.run(&["move", "500", "350"]);
    assert_eq!(watcher.next(), "500.0 350.0 test");
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(450, 250, 300, 200))
    );
    compositor.run(&["move", "--rel", "10", "10"]);
    assert_eq!(watcher.next(), "510.0 360.0 test");
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(460, 260, 300, 200))
    );
    // The clamp holds as well.
    compositor.run(&["move", "5000", "360"]);
    assert_eq!(watcher.next(), "999.0 360.0 test");
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(949, 260, 300, 200))
    );
    compositor.run(&["move", "510", "360"]);
    assert_eq!(watcher.next(), "510.0 360.0 test");

    // Clients stay out of it until the drag ends, there.
    client.sync();
    assert_eq!(client.take_input(), []);
    compositor.button(BTN_LEFT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    assert!(!compositor.with(|state| state.drag.is_some()));
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(460, 260, 300, 200))
    );
    client.sync();
    assert!(
        client
            .take_input()
            .contains(&Input::PointerEnter(dialog, 50.0, 100.0))
    );
}

#[test]
fn move_during_a_resize_drag_resizes() {
    let (compositor, mut client, _, dialog) = floating_dialog();
    compositor.pointer_at(600.0, 350.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_RIGHT, ButtonState::Pressed);
    compositor.run(&["move", "--rel", "40", "20"]);
    client.sync();
    assert_eq!(client.size(dialog), (340, 220));
    compositor.button(BTN_RIGHT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    client.sync();
    assert!(!compositor.with(|state| state.drag.is_some()));
}

#[test]
fn move_can_drop_a_tile_on_another_output() {
    let (compositor, _second) = two_outputs();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    compositor.pointer_at(500.0, 300.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    compositor.run(&["move", "1500", "300"]);
    compositor.button(BTN_LEFT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    assert_eq!(
        compositor.with(|state| state.monitors[1].workspace().windows().len()),
        1
    );
    assert_eq!(
        compositor.with(|state| state.monitors[0].workspace().windows().len()),
        0
    );
}

#[test]
fn move_with_a_button_held_on_a_client_stays_with_that_client() {
    let (compositor, mut client, a, b) = mango_like();
    compositor.pointer_at(100.0, 300.0);
    client.sync();
    assert!(client.activated(a));
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    client.take_input();

    let rect_a = compositor.frames()[0];
    compositor.run(&["move", "800", "300"]);
    client.sync();
    assert_eq!(compositor.pointer(), (800.0, 300.0));
    assert_eq!(
        client.take_input(),
        [Input::PointerMotion(
            800.0 - f64::from(rect_a.x),
            300.0 - f64::from(rect_a.y)
        )]
    );
    assert!(client.activated(a), "focus changed under a held button");
    assert!(!client.activated(b));

    // Letting go hands the pointer to the window it is on by now, without any further motion.
    compositor.button(BTN_LEFT, ButtonState::Released);
    client.sync();
    let input = client.take_input();
    assert!(input.contains(&Input::PointerLeave(a)), "{input:?}");
    assert!(
        input
            .iter()
            .any(|input| matches!(input, Input::PointerEnter(window, ..) if *window == b)),
        "{input:?}"
    );
}

#[test]
fn warps_still_wait_for_a_drag_to_end() {
    let (compositor, _client, _, _) = floating_dialog();
    compositor.pointer_at(400.0, 300.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    compositor.with(|state| state.warp_pointer((10.0, 10.0).into()));
    assert_eq!(compositor.pointer(), (400.0, 300.0));
    compositor.button(BTN_LEFT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
}

#[test]
fn move_without_any_output_fails_and_leaves_the_pointer() {
    let compositor = Compositor::start();
    compositor.pointer_at(10.0, 10.0);
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        state.remove_output(&output);
    });
    assert!(
        compositor
            .run_failing(&["move", "1", "2"])
            .contains("no output")
    );
    assert_eq!(compositor.pointer(), (10.0, 10.0));
    assert_eq!(compositor.run(&["pos", "--once"]), "10.0 10.0 -\n");
    assert_eq!(
        compositor.run(&["pos", "--once", "--json"]),
        "{\"x\":10.0,\"y\":10.0,\"output\":null}\n"
    );
}

#[test]
fn the_socket_answers_garbage_and_goes_on() {
    let compositor = Compositor::start();
    let mut ipc = compositor.raw_ipc();
    assert_eq!(ipc.request("dance"), "error unknown request");
    assert_eq!(ipc.request(""), "error unknown request");
    assert_eq!(ipc.request("move 1"), "error expected two numbers");
    assert_eq!(
        ipc.request("move nan 2"),
        "error coordinates must be finite"
    );
    assert_eq!(ipc.request("\u{fffd}\u{0}"), "error unknown request");
    assert_eq!(ipc.request("move 3 4"), "ok");
    assert_eq!(ipc.request("position"), "position 3.0 4.0 test");

    // Several requests in one write, and one spread over two.
    ipc.send("position\nmove 5 ");
    assert_eq!(ipc.line(), "position 3.0 4.0 test");
    ipc.send("6\n");
    assert_eq!(ipc.line(), "ok");
    assert_eq!(compositor.pointer(), (5.0, 6.0));
}

#[test]
fn an_endless_request_costs_the_sender_its_connection_only() {
    let compositor = Compositor::start();
    let mut flood = compositor.raw_ipc();
    flood.stream.set_nonblocking(true).unwrap();
    let chunk = [b'x'; 4096];
    let start = Instant::now();
    while flood.stream.write(&chunk).is_ok_and(|written| written > 0) {
        assert!(start.elapsed() < ANSWER, "the connection was not closed");
        compositor.with(|_| ());
    }
    assert_eq!(compositor.raw_ipc().request("move 1 2"), "ok");
}

#[test]
fn a_client_flooding_requests_without_reading_is_dropped() {
    let compositor = Compositor::start();
    let mut flood = compositor.raw_ipc();
    flood.stream.set_nonblocking(true).unwrap();
    let start = Instant::now();
    while flood
        .stream
        .write(b"position\n")
        .is_ok_and(|written| written > 0)
    {
        assert!(
            start.elapsed() < ANSWER * 6,
            "the connection was not closed"
        );
    }
    assert_eq!(
        compositor.raw_ipc().request("position"),
        "position 0.0 0.0 test"
    );
    let open = compositor.with(|state| state.ipc.client_count());
    assert!(open <= 2, "{open} clients still open");
}

#[test]
fn a_watcher_that_stops_reading_gets_the_latest_position_later() {
    let compositor = Compositor::start();
    let mut watcher = compositor.raw_ipc();
    assert_eq!(watcher.request("watch"), "position 0.0 0.0 test");

    // Far more than the socket buffer holds. The compositor must not wait for the reader.
    let total = 300_000;
    let start = Instant::now();
    compositor.with(move |state| {
        for step in 0..total {
            let x = 1.0 + f64::from(step) * 0.001;
            state.on_pointer_motion((x, 5.0).into(), InputTime::now());
        }
        state.on_pointer_motion((77.25, 5.0).into(), InputTime::now());
    });
    assert!(start.elapsed() < Duration::from_secs(60));

    // Everything that arrives is a whole line, and the last one is where the pointer ended up,
    // although nothing moved after the reader came back.
    let mut lines = 0;
    let mut previous = 0.0;
    loop {
        let line = watcher.line();
        let position = Position::parse(&line).unwrap_or_else(|| panic!("torn line: {line:?}"));
        lines += 1;
        if position.x == 77.25 {
            break;
        }
        assert!(position.x > previous, "{line}");
        previous = position.x;
    }
    assert!(
        lines < total,
        "nothing was dropped, the buffer is too large for this test"
    );

    compositor.pointer_at(78.0, 5.0);
    assert_eq!(watcher.line(), "position 78.0 5.0 test");
}

#[test]
fn a_spawned_client_finds_the_compositor_it_runs_in() {
    let compositor = Compositor::start();
    compositor.pointer_at(21.0, 22.0);
    let path = scratch_path("spawned-pos");
    let binary = env!("CARGO_BIN_EXE_wirbel");
    let command = format!("{binary} pos --once > {path:?}.tmp; mv {path:?}.tmp {path:?}");
    compositor.with(move |state| state.spawn(&command));
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < ANSWER,
            "the spawned command never finished"
        );
        compositor.with(|_| ());
    }
    assert_eq!(fs::read_to_string(&path).unwrap(), "21.0 22.0 test\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn an_exclusive_zone_makes_room_and_gives_it_back() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let bar = client.open_bar(30, BAR);
    client.sync();

    assert_eq!(client.state.layers[bar].size, (1000, 30));
    // 10 gap and 4 border below the bar, 14 at the bottom.
    assert_eq!(compositor.frames(), [Rect::new(14, 44, 972, 542)]);
    client.sync();
    assert_eq!(client.size(a), (972, 542));

    client.close_layer(bar);
    assert_eq!(compositor.frames(), [Rect::new(14, 14, 972, 572)]);
    client.sync();
    assert_eq!(client.size(a), FULL);
}

#[test]
fn exclusive_zones_of_all_edges_add_up_with_margins() {
    use zwlr_layer_surface_v1::Anchor;
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_layer(Level::Top, BAR, |layer| {
        layer.set_size(20, 0);
        layer.set_anchor(Anchor::Left | Anchor::Top | Anchor::Bottom);
        layer.set_exclusive_zone(20);
        layer.set_margin(0, 0, 0, 5);
    });
    client.open_layer(Level::Bottom, BAR, |layer| {
        layer.set_size(0, 40);
        layer.set_anchor(Anchor::Left | Anchor::Right | Anchor::Bottom);
        layer.set_exclusive_zone(40);
    });
    assert_eq!(
        compositor.with(|state| state.work_area(0)),
        Some(Rect::new(25, 0, 975, 560))
    );
    assert_eq!(compositor.frames(), [Rect::new(39, 14, 947, 532)]);
}

#[test]
fn a_zone_larger_than_the_output_leaves_the_windows_alone() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_layer(Level::Top, BAR, |layer| {
        layer.set_size(0, 30);
        layer.set_anchor(top_bar_anchor());
        layer.set_exclusive_zone(5000);
    });
    assert_eq!(compositor.frames(), [Rect::new(14, 14, 972, 572)]);
}

#[test]
fn a_surface_with_exclusive_zone_minus_one_stretches_under_a_bar() {
    use zwlr_layer_surface_v1::Anchor;
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_bar(30, BAR);
    let wallpaper = client.open_layer(Level::Background, GREEN, |layer| {
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_exclusive_zone(-1);
    });
    assert_eq!(client.state.layers[wallpaper].size, (1000, 600));
    assert_eq!(compositor.screenshot().at(500, 5), BAR);
    assert_eq!(compositor.screenshot().at(500, 300), GREEN);
}

#[test]
fn layers_stack_around_the_windows() {
    use zwlr_layer_surface_v1::Anchor;
    let full = Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right;
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_layer(Level::Background, GREEN, |layer| layer.set_anchor(full));
    // In the gap only something below the windows is seen.
    assert_eq!(compositor.screenshot().at(3, 3), GREEN);
    assert_eq!(compositor.screenshot().at(500, 300), RED);

    client.open_layer(Level::Bottom, BLUE, |layer| {
        layer.set_size(1000, 100);
        layer.set_anchor(Anchor::Top);
    });
    let shot = compositor.screenshot();
    assert_eq!(shot.at(3, 3), BLUE);
    assert_eq!(shot.at(500, 50), RED);

    client.open_layer(Level::Top, BAR, |layer| {
        layer.set_size(100, 100);
        layer.set_anchor(Anchor::Top);
    });
    client.open_layer(Level::Overlay, POPUP, |layer| {
        layer.set_size(50, 50);
        layer.set_anchor(Anchor::Top);
    });
    let shot = compositor.screenshot();
    assert_eq!(shot.at(500, 75), BAR);
    assert_eq!(shot.at(500, 25), POPUP);
}

#[test]
fn fullscreen_covers_the_top_layer_but_not_the_overlay() {
    use zwlr_layer_surface_v1::Anchor;
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let bar = client.open_bar(30, BAR);
    client.open_layer(Level::Overlay, POPUP, |layer| {
        layer.set_size(50, 50);
        layer.set_anchor(Anchor::Bottom);
    });
    client.sync();
    client.draw(a, RED);

    client.toplevel(a).set_fullscreen(None);
    client.sync();
    assert_eq!(client.size(a), (WIDTH, HEIGHT));
    client.draw(a, RED);
    let shot = compositor.screenshot();
    assert_eq!(shot.at(500, 5), RED, "the bar shows through");
    assert_eq!(shot.at(500, 575), POPUP, "the overlay is gone");

    compositor.pointer_at(500.0, 300.0);
    client.take_input();
    compositor.pointer_at(500.0, 5.0);
    assert_eq!(
        client.take_input().last(),
        Some(&Input::PointerMotion(500.0, 5.0))
    );

    client.toplevel(a).unset_fullscreen();
    client.sync();
    assert_eq!(compositor.screenshot().at(500, 5), BAR);
    let input = client.layer_input(bar);
    assert!(
        client
            .take_input()
            .contains(&Input::PointerEnter(input, 500.0, 5.0))
    );
}

#[test]
fn maximize_stays_inside_the_work_area() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    client.open_bar(30, BAR);
    client.toplevel(a).set_maximized();
    client.sync();
    assert_eq!(compositor.frames(), [Rect::new(14, 44, 972, 542)]);
}

#[test]
fn new_floating_windows_open_in_the_middle_of_the_work_area() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_bar(100, BAR);
    client.open_sized_window((200, 100), RED, |toplevel| {
        toplevel.set_min_size(200, 100);
        toplevel.set_max_size(200, 100);
    });
    assert_eq!(compositor.frames(), [Rect::new(400, 300, 200, 100)]);
}

#[test]
fn the_pointer_reaches_a_panel_and_sloppy_focus_ignores_it() {
    let compositor = Compositor::start_with("warp_cursor = 0\n");
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let bar = client.open_bar(30, BAR);
    let input = client.layer_input(bar);
    client.take_input();

    compositor.pointer_at(500.0, 300.0);
    client.take_input();
    compositor.run(&["move", "400.5", "10.25"]);
    assert_eq!(
        client.take_input(),
        [
            Input::PointerLeave(a),
            Input::PointerEnter(input, 400.5, 10.25)
        ]
    );
    assert_eq!(compositor.run(&["pos", "--once"]), "400.5 10.25 test\n");
    assert!(compositor.with(|state| state.window_at((400.0, 10.0).into()).is_none()));
    assert!(compositor.with(|state| state.focused.is_some()));
}

#[test]
fn a_panel_can_be_clicked_without_taking_the_focus() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let bar = client.open_bar(30, BAR);
    client.take_input();

    compositor.pointer_at(500.0, 300.0);
    client.take_input();
    compositor.pointer_at(500.0, 10.0);
    compositor.click();
    let input = client.take_input();
    assert!(input.contains(&Input::PointerEnter(client.layer_input(bar), 500.0, 10.0)));
    assert!(input.contains(&Input::Button(BTN_LEFT, wl_pointer::ButtonState::Pressed)));
    assert!(!input.contains(&Input::KeyboardLeave(a)));
    compositor.tap_key(KEY_A);
    assert!(
        client
            .take_input()
            .contains(&Input::Key(KEY_A, wl_keyboard::KeyState::Pressed))
    );
    assert!(client.activated(a));
}

#[test]
fn a_mouse_bind_does_not_apply_to_a_panel() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_bar(30, BAR);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.pointer_at(500.0, 10.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    assert!(compositor.with(|state| state.drag.is_none()));
    compositor.button(BTN_LEFT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
}

#[test]
fn an_exclusive_layer_holds_the_keyboard_until_it_goes() {
    use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity};
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    client.take_input();

    let launcher = client.open_layer(Level::Overlay, BAR, |layer| {
        layer.set_size(100, 100);
        layer.set_anchor(Anchor::Top);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    });
    let input = client.layer_input(launcher);
    assert_eq!(
        client.take_input(),
        [Input::KeyboardLeave(a), Input::KeyboardEnter(input)]
    );

    // Clicking a window does not take it away.
    compositor.pointer_at(100.0, 300.0);
    compositor.click();
    client.take_input();
    compositor.tap_key(KEY_A);
    assert!(
        client
            .take_input()
            .contains(&Input::Key(KEY_A, wl_keyboard::KeyState::Pressed))
    );

    client.close_layer(launcher);
    assert_eq!(client.take_input().last(), Some(&Input::KeyboardEnter(a)));
}

#[test]
fn an_exclusive_layer_gets_the_keyboard_only_with_its_first_buffer() {
    use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity};
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    client.take_input();
    let index = client.open_unmapped_layer(Level::Top, |layer| {
        layer.set_size(100, 100);
        layer.set_anchor(Anchor::Top);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    });
    assert_eq!(client.take_input(), []);
    client.draw_layer(index, BAR);
    assert_eq!(client.take_input().first(), Some(&Input::KeyboardLeave(a)));
}

#[test]
fn an_exclusive_layer_on_the_bottom_acts_on_demand() {
    use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity};
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    client.open_layer(Level::Bottom, BAR, |layer| {
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    });
    assert!(!client.take_input().contains(&Input::KeyboardLeave(a)));
}

#[test]
fn an_on_demand_layer_gets_the_keyboard_by_click_and_loses_it_the_same_way() {
    use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity};
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let panel = client.open_layer(Level::Top, BAR, |layer| {
        layer.set_size(100, 100);
        layer.set_anchor(Anchor::Top);
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
    });
    let input = client.layer_input(panel);
    client.take_input();

    compositor.pointer_at(500.0, 50.0);
    compositor.click();
    assert!(client.take_input().contains(&Input::KeyboardEnter(input)));

    compositor.pointer_at(100.0, 300.0);
    assert!(!client.take_input().contains(&Input::KeyboardLeave(input)));
    compositor.click();
    let events = client.take_input();
    assert!(events.contains(&Input::KeyboardLeave(input)));
    assert!(events.contains(&Input::KeyboardEnter(a)));
}

#[test]
fn sloppy_focus_leaves_the_keyboard_on_an_on_demand_layer() {
    use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity};
    let compositor = Compositor::start_with("warp_cursor = 0\n");
    let mut client = Client::connect(&compositor);
    let _a = client.open_window(RED);
    let _b = client.open_window(GREEN);
    let panel = client.open_layer(Level::Top, BAR, |layer| {
        layer.set_size(100, 100);
        layer.set_anchor(Anchor::Top);
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
    });
    let input = client.layer_input(panel);
    compositor.pointer_at(500.0, 50.0);
    compositor.click();
    compositor.pointer_at(100.0, 300.0);
    assert!(!client.take_input().contains(&Input::KeyboardLeave(input)));
}

#[test]
fn a_closing_window_leaves_the_keyboard_on_an_on_demand_layer() {
    use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity};
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);
    let panel = client.open_layer(Level::Top, BAR, |layer| {
        layer.set_size(100, 100);
        layer.set_anchor(Anchor::Top);
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
    });
    let input = client.layer_input(panel);
    compositor.pointer_at(500.0, 50.0);
    compositor.click();
    client.take_input();
    client.close(b);
    assert!(!client.take_input().contains(&Input::KeyboardLeave(input)));
    client.close_layer(panel);
    assert_eq!(client.take_input().last(), Some(&Input::KeyboardEnter(a)));
}

#[test]
fn a_null_buffer_unmaps_a_layer_surface_until_it_starts_over() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let bar = client.open_bar(30, BAR);
    assert_eq!(compositor.frames(), [Rect::new(14, 44, 972, 542)]);
    assert_eq!(client.state.layers[bar].configures, 1);

    let surface = client.state.layers[bar].surface.clone();
    surface.attach(None, 0, 0);
    surface.commit();
    client.sync();
    assert_eq!(compositor.frames(), [Rect::new(14, 14, 972, 572)]);
    assert_eq!(
        compositor.screenshot().at(500, 5),
        rgb(default_config().background_color)
    );

    // Unmapping forgets everything, as if the surface had just been created.
    let layer = client.state.layers[bar].layer.clone();
    layer.set_size(0, 30);
    layer.set_anchor(top_bar_anchor());
    layer.set_exclusive_zone(30);
    surface.commit();
    client.sync();
    assert_eq!(client.state.layers[bar].configures, 2);
    client.draw_layer(bar, BAR);
    assert_eq!(compositor.frames(), [Rect::new(14, 44, 972, 542)]);
    assert_eq!(compositor.screenshot().at(500, 5), BAR);
}

#[test]
fn a_panel_follows_its_output_and_goes_with_it() {
    let compositor = Compositor::start();
    let second = compositor.add_output("second", (1000, 0));
    let mut client = Client::connect(&compositor);
    client.sync();

    let shell: ZwlrLayerShellV1 = client.bind(1..=5);
    let outputs: Vec<WlOutput> = client
        .globals
        .contents()
        .clone_list()
        .into_iter()
        .filter(|global| global.interface == "wl_output")
        .map(|global| {
            client
                .globals
                .registry()
                .bind(global.name, 4, &client.handle, ())
        })
        .collect();
    client.sync();
    // Whichever of them it is, the surface must end up on exactly that output.
    let surface = client.compositor.create_surface(&client.handle, ());
    let layer = shell.get_layer_surface(
        &surface,
        Some(&outputs[1]),
        Level::Top,
        "test".into(),
        &client.handle,
        (),
    );
    layer.set_size(0, 30);
    layer.set_anchor(top_bar_anchor());
    layer.set_exclusive_zone(30);
    surface.commit();
    client.state.others.push(surface.clone());
    client.state.layers.push(LayerState {
        surface,
        layer,
        size: (0, 0),
        configures: 0,
        closed: false,
        buffer: None,
    });
    client.sync();
    client.draw_layer(0, BAR);

    let (zones, on_second) = compositor.with(|state| {
        let zones = (state.work_area(0), state.work_area(1));
        let surface = state.space.outputs().count();
        (zones, surface)
    });
    assert_eq!(on_second, 2);
    let (x, zone) = if zones.1 == Some(Rect::new(1000, 30, 1000, 570)) {
        (1500.0, 1)
    } else {
        assert_eq!(zones.0, Some(Rect::new(0, 30, 1000, 570)));
        (500.0, 0)
    };
    compositor.pointer_at(x, 10.0);
    let input = client.layer_input(0);
    assert!(
        client
            .take_input()
            .contains(&Input::PointerEnter(input, 500.0, 10.0))
    );
    assert_eq!(compositor.screenshot_of(zone).at(500, 10), BAR);

    let output = if zone == 1 {
        second
    } else {
        compositor.with(|state| state.monitors[0].output.clone())
    };
    compositor.with(move |state| state.remove_output(&output));
    client.sync();
    assert!(client.state.layers[0].closed);
}

#[test]
fn a_panel_grows_with_its_output() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let bar = client.open_bar(30, BAR);
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        let mode = Mode {
            size: (800, 400).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, None);
        state.output_resized(&output);
    });
    client.sync();
    assert_eq!(client.state.layers[bar].size, (800, 30));
    assert_eq!(compositor.frames(), [Rect::new(14, 44, 772, 342)]);
}

#[test]
fn a_layer_without_output_goes_to_the_active_one_and_with_no_output_closes() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        state.remove_output(&output);
    });
    let shell: ZwlrLayerShellV1 = client.bind(1..=5);
    let surface = client.compositor.create_surface(&client.handle, ());
    let layer = shell.get_layer_surface(
        &surface,
        None,
        Level::Top,
        "test".into(),
        &client.handle,
        (),
    );
    client.state.layers.push(LayerState {
        surface,
        layer,
        size: (0, 0),
        configures: 0,
        closed: false,
        buffer: None,
    });
    client.sync();
    assert!(client.state.layers[0].closed);
}

#[test]
fn a_dying_client_takes_its_panel_along() {
    let compositor = Compositor::start();
    let mut windows = Client::connect(&compositor);
    windows.open_window(RED);
    {
        let mut panel = Client::connect(&compositor);
        panel.open_bar(30, BAR);
        assert_eq!(compositor.frames(), [Rect::new(14, 44, 972, 542)]);
    }
    windows.sync();
    let start = Instant::now();
    while compositor.frames() != [Rect::new(14, 14, 972, 572)] {
        assert!(start.elapsed() < ANSWER, "the zone stays taken");
        windows.sync();
    }
}

#[test]
fn a_panel_popup_opens_above_windows_and_stays_on_the_output() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let bar = client.open_bar(30, BAR);
    let positioner = client.positioner((950, 20), (100, 50));
    positioner.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::SlideX);
    let surface = client.compositor.create_surface(&client.handle, ());
    let xdg_surface = client.wm_base.get_xdg_surface(&surface, &client.handle, ());
    let popup = xdg_surface.get_popup(None, &positioner, &client.handle, ());
    client.state.layers[bar].layer.get_popup(&popup);
    let popup = client.finish_popup(surface, xdg_surface, popup, (100, 50), POPUP, None);

    assert_eq!(client.state.popup_position, Some((900, 20)));
    assert_eq!(compositor.screenshot().at(950, 40), POPUP);
    compositor.pointer_at(950.0, 40.0);
    assert!(
        client
            .take_input()
            .contains(&Input::PointerEnter(popup.input, 50.0, 20.0))
    );
}

#[test]
fn a_popup_of_a_lower_layer_opens_above_the_windows() {
    use zwlr_layer_surface_v1::Anchor;
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let desktop = client.open_layer(Level::Bottom, BAR, |layer| {
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    });
    let popup = client.open_layer_popup(desktop, (400, 200), (100, 100), POPUP, None);
    assert_eq!(compositor.screenshot().at(450, 250), POPUP);
    compositor.pointer_at(450.0, 250.0);
    assert!(
        client
            .take_input()
            .contains(&Input::PointerEnter(popup.input, 50.0, 50.0))
    );
}

#[test]
fn unmapping_a_panel_withdraws_its_popups_and_their_grab() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let bar = client.open_bar(30, BAR);
    compositor.pointer_at(500.0, 10.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    let serial = client.state.button_serial;
    let _popup = client.open_layer_popup(bar, (100, 100), (100, 100), POPUP, Some(serial));
    compositor.button(BTN_LEFT, ButtonState::Released);
    assert!(compositor.with(|state| state.popup_grab.is_some()));

    let surface = client.state.layers[bar].surface.clone();
    surface.attach(None, 0, 0);
    surface.commit();
    client.sync();
    assert_eq!(client.state.popups_done, 1);
    assert!(compositor.with(|state| state.popup_grab.is_none()));
    assert!(compositor.with(|state| !state.seat.get_pointer().unwrap().is_grabbed()));
}

#[test]
fn window_popups_stay_clear_of_a_panel() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    client.open_layer(Level::Top, BAR, |layer| {
        use zwlr_layer_surface_v1::Anchor;
        layer.set_size(0, 30);
        layer.set_anchor(Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_exclusive_zone(30);
    });
    client.sync();
    client.draw(a, RED);
    let positioner = client.positioner((100, 500), (100, 200));
    positioner.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::SlideY);
    let surface = client.compositor.create_surface(&client.handle, ());
    let xdg_surface = client.wm_base.get_xdg_surface(&surface, &client.handle, ());
    let parent = client.xdg_surface(a);
    let _popup = xdg_surface.get_popup(Some(&parent), &positioner, &client.handle, ());
    surface.commit();
    client.sync();
    // The window starts at y=14, the room ends at 570.
    assert_eq!(client.state.popup_position, Some((100, 570 - 200 - 14)));
}

#[test]
fn a_grabbing_popup_gets_the_keyboard_and_gives_it_back() {
    let (compositor, mut client, a, popup) = window_with_grabbing_popup();
    compositor.tap_key(KEY_A);
    let input = client.take_input();
    assert!(input.contains(&Input::Key(KEY_A, wl_keyboard::KeyState::Pressed)));
    assert!(compositor.with(|state| state.popup_grab.is_some()));
    let focus = compositor.with(|state| {
        let keyboard = state.seat.get_keyboard().unwrap();
        keyboard.current_focus().map(|focus| {
            use smithay::{reexports::wayland_server::Resource, wayland::seat::WaylandFocus};
            focus.wl_surface().unwrap().id().protocol_id()
        })
    });
    assert_eq!(focus, Some(popup.surface.id().protocol_id()));

    popup.destroy();
    client.sync();
    compositor.housekeeping();
    assert!(compositor.with(|state| state.popup_grab.is_none()));
    assert!(client.take_input().contains(&Input::KeyboardEnter(a)));
}

#[test]
fn a_click_outside_ends_the_grab_and_counts_as_a_click() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let mut other = Client::connect(&compositor);
    let b = other.open_window(GREEN);
    compositor.pointer_at(100.0, 100.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    let serial = client.state.button_serial;
    let parent = client.xdg_surface(a);
    let _popup = client.open_popup(&parent, (100, 100), (100, 100), POPUP, Some(serial));
    compositor.button(BTN_LEFT, ButtonState::Released);
    client.sync();
    other.take_input();

    compositor.pointer_at(RIGHT_X + 50.0, 300.0);
    // Inside the grab, the other client hears nothing.
    assert_eq!(other.take_input(), []);
    compositor.click();
    client.sync();
    assert_eq!(client.state.popups_done, 1);
    let input = other.take_input();
    assert!(input.contains(&Input::KeyboardEnter(b)), "{input:?}");
    assert!(input.contains(&Input::Button(BTN_LEFT, wl_pointer::ButtonState::Pressed)));
    assert!(compositor.with(|state| state.popup_grab.is_none()));
}

#[test]
fn a_click_inside_the_popup_keeps_it_open() {
    let (compositor, mut client, _a, popup) = window_with_grabbing_popup();
    compositor.pointer_at(250.0, 250.0);
    let input = client.take_input();
    assert!(
        input.contains(&Input::PointerEnter(popup.input, 36.0, 36.0)),
        "{input:?}"
    );
    compositor.click();
    client.sync();
    assert_eq!(client.state.popups_done, 0);
    assert!(compositor.with(|state| state.popup_grab.is_some()));
}

#[test]
fn nested_popups_hand_the_grab_down_and_back() {
    let (compositor, mut client, _a, first) = window_with_grabbing_popup();
    compositor.pointer_at(220.0, 220.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    let serial = client.state.button_serial;
    let parent = first.xdg_surface.clone();
    let second = client.open_popup(&parent, (50, 50), (50, 50), BLUE, Some(serial));
    compositor.button(BTN_LEFT, ButtonState::Released);
    client.sync();

    compositor.tap_key(KEY_A);
    client.take_input();
    second.destroy();
    client.sync();
    compositor.tap_key(KEY_A);
    assert!(
        client
            .take_input()
            .contains(&Input::KeyboardEnter(first.input))
    );
    assert_eq!(client.state.popups_done, 0);
    assert!(compositor.with(|state| state.popup_grab.is_some()));
}

#[test]
fn hiding_a_window_withdraws_its_popups_and_frees_the_seat() {
    let (compositor, mut client, _a, _popup) = window_with_grabbing_popup();
    compositor.chord(&[KEY_LEFTMETA], KEY_2);
    client.sync();
    assert_eq!(compositor.active_workspace(), 1);
    assert_eq!(client.state.popups_done, 1);
    assert!(compositor.with(|state| {
        state.popup_grab.is_none() && !state.seat.get_pointer().unwrap().is_grabbed()
    }));
    assert!(!compositor.with(|state| state.seat.get_keyboard().unwrap().is_grabbed()));
}

#[test]
fn popups_without_a_grab_are_withdrawn_along_with_their_window() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let parent = client.xdg_surface(a);
    let _popup = client.open_popup(&parent, (100, 100), (100, 100), POPUP, None);
    compositor.chord(&[KEY_LEFTMETA], KEY_2);
    client.sync();
    assert_eq!(client.state.popups_done, 1);
}

#[test]
fn a_bound_focus_change_ends_the_grab() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);
    compositor.pointer_at(100.0, 100.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    let serial = client.state.button_serial;
    let parent = client.xdg_surface(a);
    let _popup = client.open_popup(&parent, (20, 20), (100, 100), POPUP, Some(serial));
    compositor.button(BTN_LEFT, ButtonState::Released);
    client.take_input();

    compositor.chord(&[KEY_LEFTALT], KEY_RIGHT);
    client.sync();
    assert_eq!(client.state.popups_done, 1);
    assert!(client.take_input().contains(&Input::KeyboardEnter(b)));
}

#[test]
fn pos_and_move_keep_working_through_a_popup_grab() {
    let (compositor, mut client, _a, popup) = window_with_grabbing_popup();
    let watcher = Watcher::start(&compositor, &[]);
    assert_eq!(watcher.next(), "100.0 100.0 test");

    compositor.run(&["move", "250", "250"]);
    assert_eq!(watcher.next(), "250.0 250.0 test");
    assert!(
        client
            .take_input()
            .contains(&Input::PointerEnter(popup.input, 36.0, 36.0))
    );

    // The first motion after the client took the popup away must not get lost.
    popup.destroy();
    client.sync();
    compositor.run(&["move", "300", "300"]);
    assert_eq!(watcher.next(), "300.0 300.0 test");
    assert_eq!(compositor.pointer(), (300.0, 300.0));
}

#[test]
fn a_grab_with_a_foreign_pointer_grab_running_is_refused() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    compositor.pointer_at(100.0, 100.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_RIGHT, ButtonState::Pressed);
    assert!(compositor.with(|state| state.drag.is_some()));
    let positioner = client.positioner((20, 20), (100, 100));
    let surface = client.compositor.create_surface(&client.handle, ());
    let xdg_surface = client.wm_base.get_xdg_surface(&surface, &client.handle, ());
    let parent = client.xdg_surface(a);
    let popup = xdg_surface.get_popup(Some(&parent), &positioner, &client.handle, ());
    popup.grab(&client.seat(), 1);
    surface.commit();
    client.sync();
    assert_eq!(client.state.popups_done, 1);
    assert!(compositor.with(|state| state.drag.is_some()));
    compositor.button(BTN_RIGHT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
}

#[test]
fn every_window_is_decorated_by_the_server_whatever_it_asks() {
    use zxdg_toplevel_decoration_v1::Mode as DecorationMode;
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let manager: ZxdgDecorationManagerV1 = client.bind(1..=1);
    let handle = client.handle.clone();
    let mut decoration = None;
    let a = client.open_unmapped_window_with(|toplevel| {
        decoration = Some(manager.get_toplevel_decoration(toplevel, &handle, ()));
    });
    assert_eq!(client.state.decoration_modes, [DecorationMode::ServerSide]);
    client.draw(a, RED);

    let decoration = decoration.unwrap();
    decoration.set_mode(DecorationMode::ClientSide);
    client.sync();
    assert_eq!(
        client.state.decoration_modes.last(),
        Some(&DecorationMode::ServerSide)
    );
    decoration.unset_mode();
    client.sync();
    assert_eq!(client.state.decoration_modes.len(), 3);
    assert_eq!(
        client.state.decoration_modes.last(),
        Some(&DecorationMode::ServerSide)
    );
}

#[test]
fn the_clipboard_travels_between_clients_with_the_keyboard() {
    let (compositor, mut first, mut second) = two_clients();
    let manager: WlDataDeviceManager = second.bind(1..=3);
    let seat = second.seat();
    let device = manager.get_data_device(&seat, &second.handle, ());
    let source = manager.create_data_source(&second.handle, ());
    source.offer("text/plain".into());
    compositor.tap_key(KEY_A);
    second.sync();
    device.set_selection(Some(&source), second.state.key_serial);
    second.sync();

    let first_manager: WlDataDeviceManager = first.bind(1..=3);
    let first_seat = first.seat();
    let _first_device = first_manager.get_data_device(&first_seat, &first.handle, ());
    first.sync();
    assert!(
        first.state.selection.is_none(),
        "offered without the keyboard"
    );

    compositor.pointer_at(100.0, 100.0);
    compositor.click();
    first.sync();
    let offer = first
        .state
        .selection
        .clone()
        .expect("no offer with the keyboard");
    let text = exchange(&mut second, &mut first, |_, fd| {
        offer.receive("text/plain".into(), fd);
    });
    assert_eq!(text, CLIPBOARD_TEXT);
}

#[test]
fn the_primary_selection_travels_too() {
    let (compositor, mut first, mut second) = two_clients();
    let manager: ZwpPrimarySelectionDeviceManagerV1 = second.bind(1..=1);
    let seat = second.seat();
    let device = manager.get_device(&seat, &second.handle, ());
    let source = manager.create_source(&second.handle, ());
    source.offer("text/plain".into());
    compositor.tap_key(KEY_A);
    second.sync();
    device.set_selection(Some(&source), second.state.key_serial);
    second.sync();

    let first_manager: ZwpPrimarySelectionDeviceManagerV1 = first.bind(1..=1);
    let first_seat = first.seat();
    let _first_device = first_manager.get_device(&first_seat, &first.handle, ());
    first.sync();
    compositor.pointer_at(100.0, 100.0);
    compositor.click();
    first.sync();
    let offer = first
        .state
        .primary
        .clone()
        .expect("no offer with the keyboard");
    let text = exchange(&mut second, &mut first, |_, fd| {
        offer.receive("text/plain".into(), fd);
    });
    assert_eq!(text, CLIPBOARD_TEXT);
}

#[test]
fn drag_and_drop_reaches_another_client_and_the_pointer_stays_visible_to_pos() {
    let (compositor, mut first, mut second) = two_clients();
    let manager: WlDataDeviceManager = first.bind(1..=3);
    let seat = first.seat();
    let device = manager.get_data_device(&seat, &first.handle, ());
    let source = manager.create_data_source(&first.handle, ());
    source.offer("text/plain".into());
    source.set_actions(wayland_client::protocol::wl_data_device_manager::DndAction::Copy);
    first.sync();

    let target_manager: WlDataDeviceManager = second.bind(1..=3);
    let target_seat = second.seat();
    let _target = target_manager.get_data_device(&target_seat, &second.handle, ());
    second.sync();

    compositor.pointer_at(100.0, 100.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    first.sync();
    let surface = first.state.windows[0].surface.clone();
    let icon = first.compositor.create_surface(&first.handle, ());
    device.start_drag(
        Some(&source),
        &surface,
        Some(&icon),
        first.state.button_serial,
    );
    let _icon_buffer = first.fill(&icon, 10, 10, BLUE);
    first.sync();

    assert_eq!(compositor.screenshot().at(105, 105), BLUE);
    let watcher = Watcher::start(&compositor, &[]);
    assert_eq!(watcher.next(), "100.0 100.0 test");
    compositor.run(&["move", "700", "300"]);
    assert_eq!(watcher.next(), "700.0 300.0 test");
    second.sync();
    assert_eq!(
        second.state.dnd,
        [DndEvent::Enter(700.0 - RIGHT_X, 300.0 - 14.0)]
    );
    assert_eq!(compositor.screenshot().at(705, 305), BLUE);

    // The target accepted the offer while handling the enter event; a roundtrip makes sure the
    // compositor has that before the drop, or it rightly cancels the drag.
    second.sync();
    compositor.button(BTN_LEFT, ButtonState::Released);
    second.sync();
    assert!(second.state.dnd.contains(&DndEvent::Drop));
    first.sync();
    assert!(first.state.source_events.contains(&SourceEvent::Dropped));
    assert!(compositor.with(|state| state.dnd_icon.is_none()));
    assert!(compositor.with(|state| !state.seat.get_pointer().unwrap().is_grabbed()));
}

#[test]
fn a_cursor_shape_reaches_the_backend() {
    use smithay::input::pointer::{CursorIcon, CursorImageStatus};
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let manager: WpCursorShapeManagerV1 = client.bind(1..=1);
    let pointer = client._pointer.clone();
    let device = manager.get_pointer(&pointer, &client.handle, ());
    compositor.pointer_at(100.0, 100.0);
    client.sync();
    device.set_shape(
        client.state.enter_serial,
        wp_cursor_shape_device_v1::Shape::Text,
    );
    client.sync();
    let status = compositor.with(|state| {
        matches!(
            state.cursor_status,
            CursorImageStatus::Named(CursorIcon::Text)
        )
    });
    assert!(status);
}

#[test]
fn a_viewport_scales_a_small_buffer_up() {
    use zwlr_layer_surface_v1::Anchor;
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let viewporter: WpViewporter = client.bind(1..=1);
    let index = client.open_unmapped_layer(Level::Top, |layer| {
        layer.set_size(100, 100);
        layer.set_anchor(Anchor::Top);
    });
    let surface = client.state.layers[index].surface.clone();
    let viewport = viewporter.get_viewport(&surface, &client.handle, ());
    viewport.set_destination(100, 100);
    let _buffer = client.fill(&surface, 10, 10, BAR);
    let shot = compositor.screenshot();
    let background = rgb(default_config().background_color);
    assert_eq!(shot.at(500, 50), BAR);
    assert_ne!(shot.at(500, 95), background, "not stretched downwards");
    assert_ne!(shot.at(545, 50), background, "not stretched sideways");
    assert_eq!(shot.at(500, 105), background);
}

#[test]
fn the_preferred_scale_comes_from_the_output_and_follows_it() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let manager: WpFractionalScaleManagerV1 = client.bind(1..=1);
    let surface = client.state.windows[a].surface.clone();
    let _scale = manager.get_fractional_scale(&surface, &client.handle, ());
    client.sync();
    assert_eq!(client.state.preferred_scale, Some(120));

    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        let scale = smithay::output::Scale::Fractional(1.5);
        output.change_current_state(None, None, Some(scale), None);
        state.output_resized(&output);
    });
    client.sync();
    assert_eq!(client.state.preferred_scale, Some(180));
}

#[test]
fn xdg_output_reports_the_logical_size() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let manager: ZxdgOutputManagerV1 = client.bind(1..=3);
    let output: WlOutput = client.bind(1..=4);
    let _xdg_output = manager.get_xdg_output(&output, &client.handle, ());
    client.sync();
    assert_eq!(client.state.logical_size, Some((WIDTH, HEIGHT)));
}

#[test]
fn a_dmabuf_the_renderer_accepts_becomes_a_buffer() {
    let compositor = Compositor::start();
    offer_dmabufs(&compositor, true);
    let mut client = Client::connect(&compositor);
    create_dmabuf(&mut client);
    assert_eq!(client.state.dmabuf, Some(Ok(())));
}

#[test]
fn a_dmabuf_the_renderer_refuses_fails_cleanly() {
    let compositor = Compositor::start();
    offer_dmabufs(&compositor, false);
    let mut client = Client::connect(&compositor);
    create_dmabuf(&mut client);
    assert_eq!(client.state.dmabuf, Some(Err(())));
}

#[test]
fn without_a_backend_there_are_no_dmabufs_to_get() {
    let compositor = Compositor::start();
    let client = Client::connect(&compositor);
    let list = client.globals.contents().clone_list();
    assert!(
        !list
            .iter()
            .any(|global| global.interface == "zwp_linux_dmabuf_v1")
    );
}

#[test]
fn locked_is_only_said_once_a_blank_frame_was_presented() {
    let compositor = Compositor::start();
    let mut windows = Client::connect(&compositor);
    windows.open_window(RED);
    let mut locker = Client::connect(&compositor);
    let lock = locker.lock();
    assert_eq!(locker.state.lock_events, []);

    // The window is gone from the very next frame, before any lock surface exists.
    let background = rgb(default_config().background_color);
    assert_eq!(compositor.screenshot().at(500, 300), background);
    assert_eq!(locker.state.lock_events, []);
    compositor.present();
    locker.sync();
    assert_eq!(locker.state.lock_events, [LockEvent::Locked]);

    let output = bound_output(&mut locker);
    locker.lock_surface(&lock, &output, LOCK);
    assert_eq!(locker.state.lock_size, Some((WIDTH as u32, HEIGHT as u32)));
    assert_eq!(compositor.screenshot().at(500, 300), LOCK);
}

#[test]
fn a_locked_session_shows_no_panel_and_no_window() {
    let compositor = Compositor::start();
    let mut panel = Client::connect(&compositor);
    panel.open_bar(30, BAR);
    panel.open_window(RED);
    let mut locker = Client::connect(&compositor);
    locker.lock();
    let shot = compositor.screenshot();
    let background = rgb(default_config().background_color);
    assert_eq!(shot.at(500, 5), background);
    assert_eq!(shot.at(500, 300), background);
}

#[test]
fn only_the_lock_surface_gets_keys_and_binds_are_off() {
    let (compositor, mut windows, mut locker, _lock, surface) = locked_session();
    locker.sync();
    let input = locker.take_input();
    assert!(
        input.contains(&Input::KeyboardEnter(surface.input)),
        "{input:?}"
    );
    assert_eq!(windows.take_input(), []);

    // Super+2 switches workspaces, Super+Return starts a program, Super+M quits.
    compositor.chord(&[KEY_LEFTMETA], KEY_2);
    assert_eq!(compositor.active_workspace(), 0);
    let keys = locker.take_input();
    assert!(
        keys.contains(&Input::Key(KEY_2, wl_keyboard::KeyState::Pressed)),
        "{keys:?}"
    );
    assert_eq!(windows.take_input(), []);
}

#[test]
fn the_window_loses_the_keyboard_when_the_lock_begins() {
    let compositor = Compositor::start();
    let mut windows = Client::connect(&compositor);
    let a = windows.open_window(RED);
    compositor.pointer_at(100.0, 100.0);
    windows.take_input();
    let mut locker = Client::connect(&compositor);
    locker.lock();
    let input = windows.take_input();
    assert!(input.contains(&Input::KeyboardLeave(a)), "{input:?}");
    assert!(input.contains(&Input::PointerLeave(a)), "{input:?}");
    compositor.tap_key(KEY_A);
    compositor.pointer_at(200.0, 200.0);
    compositor.click();
    assert_eq!(windows.take_input(), []);
}

#[test]
fn move_and_pos_keep_working_but_only_the_lock_surface_sees_the_pointer() {
    let (compositor, mut windows, mut locker, _lock, surface) = locked_session();
    locker.take_input();
    let watcher = Watcher::start(&compositor, &[]);
    assert_eq!(watcher.next(), "0.0 0.0 test");

    compositor.run(&["move", "300.5", "200.25"]);
    assert_eq!(watcher.next(), "300.5 200.25 test");
    compositor.run(&["move", "--rel", "1", "1"]);
    assert_eq!(watcher.next(), "301.5 201.25 test");
    assert_eq!(compositor.run(&["pos", "--once"]), "301.5 201.25 test\n");

    let input = locker.take_input();
    let _ = surface;
    assert_eq!(
        input,
        [
            Input::PointerMotion(300.5, 200.25),
            Input::PointerMotion(301.5, 201.25)
        ]
    );
    assert_eq!(windows.take_input(), []);
}

#[test]
fn clicks_of_a_locked_session_neither_focus_nor_raise_nor_drag() {
    let (compositor, mut windows, mut locker, _lock, _surface) = locked_session();
    let a = compositor.with(|state| state.focused.clone());
    compositor.pointer_at(500.0, 300.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    assert!(compositor.with(|state| state.drag.is_none()));
    compositor.button(BTN_LEFT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    assert!(compositor.with(move |state| state.focused == a));
    assert_eq!(windows.take_input(), []);
    let input = locker.take_input();
    assert!(input.contains(&Input::Button(BTN_LEFT, wl_pointer::ButtonState::Pressed)));
}

#[test]
fn locking_takes_the_pointer_away_from_a_held_button() {
    let compositor = Compositor::start();
    let mut windows = Client::connect(&compositor);
    let a = windows.open_window(RED);
    compositor.pointer_at(100.0, 100.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    windows.take_input();
    let mut locker = Client::connect(&compositor);
    locker.lock();
    assert!(windows.take_input().contains(&Input::PointerLeave(a)));
    compositor.run(&["move", "200", "200"]);
    compositor.button(BTN_LEFT, ButtonState::Released);
    assert_eq!(windows.take_input(), []);
    assert_eq!(compositor.pointer(), (200.0, 200.0));
}

#[test]
fn locking_ends_grabs_and_drags() {
    let (compositor, mut client, _a, _popup) = window_with_grabbing_popup();
    let mut locker = Client::connect(&compositor);
    locker.lock();
    client.sync();
    assert_eq!(client.state.popups_done, 1);
    assert!(compositor.with(|state| !state.seat.get_keyboard().unwrap().is_grabbed()));

    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    compositor.pointer_at(100.0, 100.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_RIGHT, ButtonState::Pressed);
    assert!(compositor.with(|state| state.drag.is_some()));
    let mut locker = Client::connect(&compositor);
    locker.lock();
    assert!(compositor.with(|state| state.drag.is_none()));
    assert!(compositor.with(|state| !state.seat.get_pointer().unwrap().is_grabbed()));
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    compositor.button(BTN_RIGHT, ButtonState::Released);
}

#[test]
fn a_grab_during_the_lock_is_refused() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    compositor.pointer_at(100.0, 100.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    let serial = client.state.button_serial;
    let mut locker = Client::connect(&compositor);
    locker.lock();
    let parent = client.xdg_surface(a);
    let _popup = client.open_popup(&parent, (20, 20), (100, 100), POPUP, Some(serial));
    client.sync();
    assert_eq!(client.state.popups_done, 1);
    assert!(compositor.with(|state| state.popup_grab.is_none()));
}

#[test]
fn a_new_window_during_the_lock_gets_nothing() {
    let (compositor, mut windows, mut locker, _lock, surface) = locked_session();
    locker.take_input();
    let b = windows.open_window(GREEN);
    assert_eq!(compositor.screenshot().at(800, 300), LOCK);
    compositor.tap_key(KEY_A);
    assert!(!windows.take_input().contains(&Input::KeyboardEnter(b)));
    let _ = surface;
    assert!(
        locker
            .take_input()
            .contains(&Input::Key(KEY_A, wl_keyboard::KeyState::Pressed))
    );
}

#[test]
fn unlocking_brings_screen_and_input_back() {
    let (compositor, mut windows, mut locker, lock, surface) = locked_session();
    compositor.pointer_at(100.0, 100.0);
    locker.take_input();
    surface.lock_surface.destroy();
    surface.surface.destroy();
    lock.unlock_and_destroy();
    locker.sync();
    assert!(!compositor.with(|state| state.lock.is_active()));
    let input = windows.take_input();
    assert!(input.contains(&Input::KeyboardEnter(0)), "{input:?}");
    assert!(
        input.contains(&Input::PointerEnter(0, 86.0, 86.0)),
        "{input:?}"
    );
    assert_eq!(compositor.screenshot().at(100, 100), RED);
    compositor.chord(&[KEY_LEFTMETA], KEY_2);
    assert_eq!(compositor.active_workspace(), 1);
}

#[test]
fn a_dead_locker_leaves_the_session_locked_for_the_next_one() {
    let (compositor, mut windows, locker, lock, surface) = locked_session();
    drop((lock, surface, locker));
    let start = Instant::now();
    while compositor.with(|state| state.lock.surfaces().count()) != 0 {
        assert!(start.elapsed() < ANSWER, "the lock surface stays");
    }
    let background = rgb(default_config().background_color);
    assert_eq!(compositor.screenshot().at(500, 300), background);
    compositor.tap_key(KEY_A);
    compositor.pointer_at(100.0, 100.0);
    compositor.click();
    assert_eq!(windows.take_input(), []);
    assert_eq!(compositor.active_workspace(), 0);

    let mut next = Client::connect(&compositor);
    let lock = next.lock();
    assert_eq!(next.state.lock_events, [LockEvent::Locked]);
    let output = bound_output(&mut next);
    let surface = next.lock_surface(&lock, &output, LOCK);
    assert_eq!(compositor.screenshot().at(500, 300), LOCK);
    assert!(
        next.take_input()
            .contains(&Input::KeyboardEnter(surface.input))
    );

    surface.lock_surface.destroy();
    lock.unlock_and_destroy();
    next.sync();
    assert_eq!(compositor.screenshot().at(500, 300), RED);
}

#[test]
fn a_second_locker_is_turned_away_while_the_first_lives() {
    let (compositor, _windows, mut locker, _lock, _surface) = locked_session();
    let mut other = Client::connect(&compositor);
    let _second = other.lock();
    assert_eq!(other.state.lock_events, [LockEvent::Finished]);
    locker.sync();
    assert_eq!(locker.state.lock_events, [LockEvent::Locked]);
}

#[test]
fn a_second_locker_is_turned_away_before_the_first_is_confirmed() {
    let compositor = Compositor::start();
    let mut first = Client::connect(&compositor);
    let _first_lock = first.lock();
    let mut other = Client::connect(&compositor);
    let _second = other.lock();
    assert_eq!(other.state.lock_events, [LockEvent::Finished]);
    compositor.present();
    first.sync();
    assert_eq!(first.state.lock_events, [LockEvent::Locked]);
}

#[test]
fn every_output_waits_for_its_blank_frame_and_keeps_its_lock_surface() {
    let compositor = Compositor::start();
    compositor.add_output("second", (1000, 0));
    let mut locker = Client::connect(&compositor);
    let lock = locker.lock();

    let first_output = compositor.with(|state| state.monitors[0].output.clone());
    compositor.with(move |state| state.send_frame_callbacks(&first_output));
    locker.sync();
    assert_eq!(locker.state.lock_events, []);
    let second_output = compositor.with(|state| state.monitors[1].output.clone());
    compositor.with(move |state| state.send_frame_callbacks(&second_output));
    locker.sync();
    assert_eq!(locker.state.lock_events, [LockEvent::Locked]);

    let outputs: Vec<WlOutput> = locker
        .globals
        .contents()
        .clone_list()
        .into_iter()
        .filter(|global| global.interface == "wl_output")
        .map(|global| {
            locker
                .globals
                .registry()
                .bind(global.name, 4, &locker.handle, ())
        })
        .collect();
    locker.sync();
    let a = locker.lock_surface(&lock, &outputs[0], LOCK);
    let b = locker.lock_surface(&lock, &outputs[1], LOCK);
    locker.take_input();

    compositor.pointer_at(1500.0, 300.0);
    let on_b = locker.take_input();
    compositor.pointer_at(500.0, 300.0);
    let on_a = locker.take_input();
    // The surfaces are on different outputs, so each side enters a different one, and the
    // keyboard goes where the pointer goes.
    let entered = |input: &[Input]| {
        input.iter().find_map(|event| match event {
            Input::KeyboardEnter(index) => Some(*index),
            _ => None,
        })
    };
    let (first, second) = (entered(&on_b), entered(&on_a));
    assert!(first.is_some() && second.is_some(), "{on_b:?} {on_a:?}");
    assert_ne!(first, second);
    assert!([a.input, b.input].contains(&first.unwrap()));
    assert!([a.input, b.input].contains(&second.unwrap()));
}

#[test]
fn removing_the_last_waiting_output_confirms_the_lock() {
    let compositor = Compositor::start();
    let second = compositor.add_output("second", (1000, 0));
    let mut locker = Client::connect(&compositor);
    let _lock = locker.lock();
    let first = compositor.with(|state| state.monitors[0].output.clone());
    compositor.with(move |state| state.send_frame_callbacks(&first));
    locker.sync();
    assert_eq!(locker.state.lock_events, []);
    compositor.with(move |state| state.remove_output(&second));
    locker.sync();
    assert_eq!(locker.state.lock_events, [LockEvent::Locked]);
}

#[test]
fn a_lock_surface_follows_the_size_of_its_output() {
    let (compositor, _windows, mut locker, _lock, _surface) = locked_session();
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        let mode = Mode {
            size: (800, 400).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, None);
        state.output_resized(&output);
    });
    locker.sync();
    assert_eq!(locker.state.lock_size, Some((800, 400)));
}

#[test]
fn the_session_goes_idle_and_input_wakes_it() {
    let compositor = Compositor::start();
    let (mut client, _notification) = idle_client(&compositor);
    wait_for_idle(&mut client, 1);
    assert_eq!(client.state.idle, [true]);
    compositor.pointer_at(10.0, 10.0);
    wait_for_idle(&mut client, 2);
    assert_eq!(client.state.idle, [true, false]);
}

#[test]
fn a_visible_inhibitor_holds_off_idling_and_a_hidden_one_does_not() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let manager: ZwpIdleInhibitManagerV1 = client.bind(1..=1);
    let surface = client.state.windows[a].surface.clone();
    let _inhibitor = manager.create_inhibitor(&surface, &client.handle, ());
    client.sync();
    compositor.housekeeping();
    assert!(compositor.with(|state| state.idle_notifier_state.is_inhibited()));
    // The notification only now: its 20 ms may run out on a busy machine before the window is up.
    let notifier: ExtIdleNotifierV1 = client.bind(1..=2);
    let seat = client.seat();
    let _notification = notifier.get_idle_notification(20, &seat, &client.handle, ());
    client.sync();
    compositor.pointer_at(10.0, 10.0);
    assert!(compositor.with(|state| state.idle_notifier_state.is_inhibited()));
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(100) {
        client.sync();
    }
    assert!(
        !client.state.idle.contains(&true),
        "{:?}",
        client.state.idle
    );

    compositor.chord(&[KEY_LEFTMETA], KEY_2);
    compositor.housekeeping();
    assert!(!compositor.with(|state| state.idle_notifier_state.is_inhibited()));
    wait_for_idle(&mut client, 1);
}

/// Taking over the screen needs a VT on stdin. This also keeps tests off the DRM backend.
#[test]
fn refuses_the_drm_backend_without_a_virtual_terminal() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_wirbel"))
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("not a virtual terminal"), "{message}");
}

#[test]
fn ctrl_alt_f_switches_the_vt_and_no_client_sees_it() {
    let compositor = Compositor::start();
    let switches = record_vt_switches(&compositor);
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.take_input();

    compositor.chord(&[KEY_LEFTCTRL, KEY_LEFTALT], KEY_F3);
    assert_eq!(*switches.lock().unwrap(), [3]);
    let input = client.take_input();
    let f_key = |event: &Input| matches!(event, Input::Key(KEY_F3, _));
    assert!(!input.iter().any(f_key), "{input:?}");
}

#[test]
fn a_locked_session_can_leave_for_another_vt_and_stays_locked() {
    let (compositor, _windows, mut locker, _lock, _surface) = locked_session();
    let switches = record_vt_switches(&compositor);
    locker.take_input();

    compositor.chord(&[KEY_LEFTCTRL, KEY_LEFTALT], KEY_F3);
    assert_eq!(*switches.lock().unwrap(), [3]);
    let input = locker.take_input();
    let f_key = |event: &Input| matches!(event, Input::Key(KEY_F3, _));
    assert!(!input.iter().any(f_key), "{input:?}");
    assert!(compositor.with(|state| state.lock.is_active()));
    assert_eq!(compositor.screenshot().at(500, 300), LOCK);
}

#[test]
fn nested_there_is_no_vt_and_the_key_stays_with_the_client() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.take_input();

    compositor.chord(&[KEY_LEFTCTRL, KEY_LEFTALT], KEY_F3);
    let input = client.take_input();
    let press = Input::Key(KEY_F3, wl_keyboard::KeyState::Pressed);
    assert!(input.contains(&press), "{input:?}");
}

#[test]
fn the_emergency_chord_works_with_no_bind_for_it_and_while_locked() {
    let compositor = Compositor::start();
    let count = record_emergency(&compositor);
    let chord = [KEY_LEFTCTRL, KEY_LEFTALT, KEY_LEFTSHIFT];
    compositor.chord(&chord[..2], KEY_BACKSPACE);
    assert_eq!(*count.lock().unwrap(), 0);
    compositor.chord(&chord, KEY_BACKSPACE);
    assert_eq!(*count.lock().unwrap(), 1);

    let (compositor, _windows, _locker, _lock, _surface) = locked_session();
    let count = record_emergency(&compositor);
    compositor.chord(&chord, KEY_BACKSPACE);
    assert_eq!(*count.lock().unwrap(), 1);
}

#[test]
fn a_frame_from_before_the_lock_does_not_confirm_it() {
    let compositor = Compositor::start();
    let mut locker = Client::connect(&compositor);
    locker.lock();
    // What the DRM backend does when the vblank of an older frame arrives.
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        state.send_frames(&output);
    });
    locker.sync();
    assert_eq!(locker.state.lock_events, []);
    compositor.present();
    locker.sync();
    assert_eq!(locker.state.lock_events, [LockEvent::Locked]);
}

#[test]
fn named_cursors_are_drawn_when_nobody_else_does_it_also_while_locked() {
    let compositor = Compositor::start();
    let background = rgb(default_config().background_color);
    compositor.pointer_at(300.0, 200.0);
    let shot = compositor.screenshot();
    assert!(!differs_from_background(&shot, background, 300, 200));

    compositor.with(|state| {
        state.cursors = Some(wirbel::cursor::Cursors::load(
            Some("no-such-theme"),
            Some(24),
        ));
    });
    let shot = compositor.screenshot();
    assert!(differs_from_background(&shot, background, 300, 200));

    let mut locker = Client::connect(&compositor);
    locker.lock();
    let shot = compositor.screenshot();
    assert!(differs_from_background(&shot, background, 300, 200));
}

#[test]
fn a_relative_pointer_gets_the_motion_as_the_mouse_made_it() {
    let (compositor, mut client, _, _) = window_for_game();
    let manager = client.bind::<ZwpRelativePointerManagerV1>(1..=1);
    let _relative = manager.get_relative_pointer(&client._pointer, &client.handle, ());
    client.sync();

    compositor.pointer_delta(3.0, -4.0);
    client.sync();
    assert_eq!(client.state.relative, [(3.0, -4.0, 6.0, -8.0)]);
    assert_eq!(compositor.pointer_location(), (103.0, 96.0));
}

#[test]
fn a_locked_pointer_stays_while_the_mouse_keeps_reporting() {
    let (compositor, mut client, constraints, window) = window_for_game();
    let manager = client.bind::<ZwpRelativePointerManagerV1>(1..=1);
    let _relative = manager.get_relative_pointer(&client._pointer, &client.handle, ());
    let surface = client.state.windows[window].surface.clone();
    let _lock = constraints.lock_pointer(
        &surface,
        &client._pointer,
        None,
        Lifetime::Persistent,
        &client.handle,
        (),
    );
    client.sync();
    client.sync();
    assert_eq!(client.state.constraint_events, [true]);

    compositor.pointer_delta(50.0, 50.0);
    client.sync();
    assert_eq!(compositor.pointer_location(), (100.0, 100.0));
    assert_eq!(client.state.relative.len(), 1);
    assert!(
        !client
            .take_input()
            .iter()
            .any(|input| matches!(input, Input::PointerMotion(..)))
    );
}

#[test]
fn a_lock_takes_the_pointer_to_the_hint_when_it_ends() {
    let (compositor, mut client, constraints, window) = window_for_game();
    let surface = client.state.windows[window].surface.clone();
    let lock = constraints.lock_pointer(
        &surface,
        &client._pointer,
        None,
        Lifetime::Persistent,
        &client.handle,
        (),
    );
    client.sync();
    lock.set_cursor_position_hint(30.0, 40.0);
    surface.commit();
    client.sync();
    lock.destroy();
    client.sync();
    client.sync();

    let origin = compositor.focused_rect().unwrap();
    let expected = (f64::from(origin.x) + 30.0, f64::from(origin.y) + 40.0);
    assert_eq!(compositor.pointer_location(), expected);
}

#[test]
fn a_confined_pointer_slides_along_the_edge_of_its_region() {
    let (compositor, mut client, constraints, window) = window_for_game();
    let rect = compositor.focused_rect().unwrap();
    let local = |x: f64, y: f64| (f64::from(rect.x) + x, f64::from(rect.y) + y);
    let start = local(50.0, 50.0);
    compositor.pointer_at(start.0, start.1);

    let surface = client.state.windows[window].surface.clone();
    let region = client.compositor.create_region(&client.handle, ());
    region.add(0, 0, 100, 100);
    let _confined = constraints.confine_pointer(
        &surface,
        &client._pointer,
        Some(&region),
        Lifetime::Persistent,
        &client.handle,
        (),
    );
    surface.commit();
    client.sync();
    client.sync();
    assert_eq!(client.state.constraint_events, [true]);

    compositor.pointer_delta(10.0, 0.0);
    assert_eq!(compositor.pointer_location(), local(60.0, 50.0));
    // Past the right edge, but free to go down.
    compositor.pointer_delta(100.0, 5.0);
    assert_eq!(compositor.pointer_location(), local(60.0, 55.0));
    compositor.pointer_delta(100.0, 100.0);
    assert_eq!(compositor.pointer_location(), local(60.0, 55.0));
}

#[test]
fn a_window_without_the_keyboard_cannot_catch_the_pointer() {
    let (compositor, mut client, constraints, first) = window_for_game();
    let second = client.open_window(0x00ff00);
    client.sync();
    let rects = compositor.frames();
    let left = rects[0];
    compositor.pointer_at(f64::from(left.x + 10), f64::from(left.y + 10));
    assert_ne!(first, second);

    // The pointer is over the first window, the second one has the keyboard.
    let surface = client.state.windows[first].surface.clone();
    let _lock = constraints.lock_pointer(
        &surface,
        &client._pointer,
        None,
        Lifetime::Persistent,
        &client.handle,
        (),
    );
    client.sync();
    client.sync();
    assert_eq!(client.state.constraint_events, []);

    compositor.click();
    client.sync();
    client.sync();
    assert_eq!(client.state.constraint_events, [true]);

    compositor.chord(&[KEY_LEFTALT], KEY_RIGHT);
    client.sync();
    client.sync();
    assert_eq!(client.state.constraint_events, [true, false]);
}

#[test]
fn an_absolute_device_moves_a_locked_pointer_by_relative_motion() {
    let (compositor, mut client, constraints, window) = window_for_game();
    let manager = client.bind::<ZwpRelativePointerManagerV1>(1..=1);
    let _relative = manager.get_relative_pointer(&client._pointer, &client.handle, ());
    // Unconstrained, the host cursor puts the pointer where it is, without relative motion.
    compositor.pointer_absolute(100.0, 100.0);
    let surface = client.state.windows[window].surface.clone();
    let _lock = constraints.lock_pointer(
        &surface,
        &client._pointer,
        None,
        Lifetime::Persistent,
        &client.handle,
        (),
    );
    client.sync();
    client.sync();
    assert_eq!(client.state.constraint_events, [true]);
    client.take_input();

    compositor.pointer_absolute(112.0, 95.0);
    compositor.pointer_absolute(115.0, 99.0);
    client.sync();
    assert_eq!(
        client.state.relative,
        [(12.0, -5.0, 12.0, -5.0), (3.0, 4.0, 3.0, 4.0)]
    );
    assert_eq!(compositor.pointer_location(), (100.0, 100.0));
    assert!(
        !client
            .take_input()
            .iter()
            .any(|input| matches!(input, Input::PointerMotion(..)))
    );
}

#[test]
fn a_locked_pointer_is_not_moved_on_its_own_surface() {
    let (compositor, mut client, constraints, window) = window_for_game();
    let surface = client.state.windows[window].surface.clone();
    let _lock = constraints.lock_pointer(
        &surface,
        &client._pointer,
        None,
        Lifetime::Persistent,
        &client.handle,
        (),
    );
    client.sync();
    client.sync();
    client.take_input();
    compositor.pointer_at(130.0, 120.0);
    compositor.with(|state| state.refresh_pointer_focus());
    client.sync();
    assert_eq!(compositor.pointer_location(), (100.0, 100.0));
    assert_eq!(client.state.constraint_events, [true]);
    assert!(
        !client
            .take_input()
            .iter()
            .any(|input| matches!(input, Input::PointerMotion(..)))
    );
}

#[test]
fn a_lock_that_never_held_leaves_the_pointer_where_it_is() {
    let (compositor, mut client, constraints, first) = window_for_game();
    let second = client.open_window(0x00ff00);
    client.sync();
    let left = compositor.frames()[0];
    let start = (f64::from(left.x + 10), f64::from(left.y + 10));
    compositor.pointer_at(start.0, start.1);
    assert_ne!(first, second);

    // The second window has the keyboard, so the lock on the first one waits.
    let surface = client.state.windows[first].surface.clone();
    let lock = constraints.lock_pointer(
        &surface,
        &client._pointer,
        None,
        Lifetime::Persistent,
        &client.handle,
        (),
    );
    lock.set_cursor_position_hint(50.0, 60.0);
    surface.commit();
    client.sync();
    lock.destroy();
    client.sync();
    client.sync();
    assert_eq!(client.state.constraint_events, []);
    assert_eq!(compositor.pointer_location(), start);
}

#[test]
fn moving_the_pointer_away_by_command_ends_a_lock() {
    let (compositor, mut client, constraints, window) = window_for_game();
    let surface = client.state.windows[window].surface.clone();
    let _lock = constraints.lock_pointer(
        &surface,
        &client._pointer,
        None,
        Lifetime::Persistent,
        &client.handle,
        (),
    );
    client.sync();
    client.sync();
    compositor.pointer_at(2.0, 2.0);
    client.sync();
    assert_eq!(client.state.constraint_events, [true, false]);
}

#[test]
fn a_token_from_a_key_press_brings_up_the_window_on_its_workspace() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let far = client.open_window(0xff0000);
    compositor.chord(&[KEY_LEFTMETA], KEY_2);
    assert_eq!(compositor.active_workspace(), 1);
    let near = client.open_window(0x00ff00);
    client.sync();
    compositor.tap_key(KEY_A);
    client.sync();

    let surface = client.state.windows[near].surface.clone();
    let token = activation_token(&mut client, true, &surface);
    let activation = client.bind::<XdgActivationV1>(1..=1);
    activation.activate(token, &client.state.windows[far].surface);
    client.sync();
    client.sync();
    assert_eq!(compositor.active_workspace(), 0);
    assert!(client.state.windows[far].activated);
}

#[test]
fn a_window_that_only_wants_attention_gets_no_focus() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let far = client.open_window(0xff0000);
    let near = client.open_window(0x00ff00);
    client.sync();

    let surface = client.state.windows[far].surface.clone();
    let token = activation_token(&mut client, false, &surface);
    let activation = client.bind::<XdgActivationV1>(1..=1);
    activation.activate(token, &client.state.windows[far].surface);
    client.sync();
    client.sync();
    assert!(client.state.windows[near].activated);
    assert!(!client.state.windows[far].activated);
}

#[test]
fn presentation_feedback_answers_with_the_frame_that_shows_the_surface() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let window = client.open_window(0xff0000);
    let presentation = client.bind::<WpPresentation>(1..=1);

    let surface = client.state.windows[window].surface.clone();
    let _feedback = presentation.feedback(&surface, &client.handle, ());
    surface.commit();
    client.sync();

    compositor.with(|state| {
        state.space.refresh();
        let output = state.monitors[0].output.clone();
        let mut renderer = PixmanRenderer::new().unwrap();
        let size = Size::<i32, Buffer>::from((WIDTH, HEIGHT));
        let mut image: Image<'static, 'static> =
            renderer.create_buffer(Fourcc::Argb8888, size).unwrap();
        let mut framebuffer = renderer.bind(&mut image).unwrap();
        let elements = state.scene_elements(&mut renderer, &output);
        let mut tracker = OutputDamageTracker::from_output(&output);
        let result = tracker
            .render_output(&mut renderer, &mut framebuffer, 0, &elements, [0.0; 4])
            .unwrap();
        state.update_scanout_outputs(&output, &result.states);
        let mut feedback = state.presentation_feedback(&output, &result.states);
        let refresh = Refresh::fixed(Duration::from_millis(16));
        let kind = server_feedback::Kind::Vsync;
        feedback.presented(state.clock.now(), refresh, 7, kind);
    });
    client.sync();
    assert_eq!(client.state.presented, [true]);
}

#[test]
fn an_x11_window_is_tiled_gets_the_keyboard_and_closes() {
    let Some((compositor, connection, root, _turn)) = with_x_server() else {
        return;
    };
    let window = x_window(&connection, root, false);
    wait_for(|| compositor.focused_rect().is_some());

    let rect = compositor.focused_rect().unwrap();
    wait_for(|| {
        let wanted = (rect.x, rect.y, rect.w as u32, rect.h as u32);
        x_geometry(&connection, window) == wanted
    });
    assert!(rect.w > 900, "not tiled: {rect:?}");

    compositor.tap_key(KEY_A);
    let deadline = Instant::now() + Duration::from_secs(10);
    let press = loop {
        match connection.poll_for_event().unwrap() {
            Some(Event::KeyPress(press)) => break press,
            _ => {
                assert!(Instant::now() < deadline, "no key press");
                thread::sleep(Duration::from_millis(10));
            }
        }
    };
    assert_eq!(press.detail, (KEY_A + 8) as u8);

    connection.destroy_window(window).unwrap();
    connection.flush().unwrap();
    wait_for(|| compositor.focused_rect().is_none());
    assert_eq!(compositor.frames(), []);
}

#[test]
fn x11_and_wayland_windows_share_a_workspace() {
    let Some((compositor, connection, root, _turn)) = with_x_server() else {
        return;
    };
    let mut client = Client::connect(&compositor);
    client.open_window(0x00ff00);
    let window = x_window(&connection, root, false);
    wait_for(|| compositor.frames().len() == 2);
    wait_for(|| {
        let rects = compositor.frames();
        let (x, y, w, h) = x_geometry(&connection, window);
        rects
            .iter()
            .any(|r| (r.x, r.y, r.w, r.h) == (x, y, w as i32, h as i32))
    });
    // The newcomer has the keyboard, and the screen shows both.
    wait_for(|| {
        compositor.with(|state| {
            let keyboard = state.seat.get_keyboard().unwrap();
            matches!(keyboard.current_focus(), Some(KeyboardFocus::X11(_)))
        })
    });
}

#[test]
fn an_x11_menu_stays_out_of_the_tiling() {
    let Some((compositor, connection, root, _turn)) = with_x_server() else {
        return;
    };
    let mut client = Client::connect(&compositor);
    let wayland = client.open_window(0x00ff00);
    let rect = compositor.focused_rect().unwrap();
    let menu = x_window(&connection, root, true);
    wait_for(|| compositor.with(|state| state.overrides.len() == 1));

    assert_eq!(compositor.frames(), [rect]);
    assert_eq!(compositor.focused_rect(), Some(rect));
    assert!(client.state.windows[wayland].activated);
    let (x, y) = compositor.with(|state| {
        let window = &state.overrides[0];
        let location = state.space.element_location(window).unwrap();
        (location.x, location.y)
    });
    assert_eq!((x, y), (40, 30));

    connection.unmap_window(menu).unwrap();
    connection.flush().unwrap();
    wait_for(|| compositor.with(|state| state.overrides.is_empty()));
}

#[test]
fn an_x11_window_can_go_fullscreen() {
    let Some((compositor, connection, root, _turn)) = with_x_server() else {
        return;
    };
    let window = x_window(&connection, root, false);
    wait_for(|| compositor.focused_rect().is_some());

    let atom = |name: &[u8]| {
        connection
            .intern_atom(false, name)
            .unwrap()
            .reply()
            .unwrap()
            .atom
    };
    let (state_atom, full) = (atom(b"_NET_WM_STATE"), atom(b"_NET_WM_STATE_FULLSCREEN"));
    let event = ClientMessageEvent::new(32, window, state_atom, [1, full, 0, 1, 0]);
    connection
        .send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )
        .unwrap();
    connection.flush().unwrap();
    wait_for(|| x_geometry(&connection, window) == (0, 0, WIDTH as u32, HEIGHT as u32));
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(0, 0, WIDTH, HEIGHT))
    );
}

/// Games on X11 grab the pointer, and Xwayland turns that into a pointer constraint.
#[test]
fn an_x11_pointer_grab_confines_the_pointer() {
    let Some((compositor, connection, root, _turn)) = with_x_server() else {
        return;
    };
    let window = x_window(&connection, root, false);
    wait_for(|| compositor.focused_rect().is_some());
    compositor.pointer_at(100.0, 100.0);

    let grab = |confine: u32| {
        connection
            .grab_pointer(
                false,
                window,
                EventMask::POINTER_MOTION,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                confine,
                0u32,
                0u32,
            )
            .unwrap()
            .reply()
            .unwrap()
            .status
    };
    wait_for(|| grab(window) == GrabStatus::SUCCESS);
    let constrained = || {
        compositor.with(|state| {
            let pointer = state.seat.get_pointer().unwrap();
            let Some(surface) = pointer.current_focus() else {
                return false;
            };
            with_pointer_constraint(&surface, &pointer, |constraint| {
                constraint.is_some_and(|constraint| constraint.is_active())
            })
        })
    };
    wait_for(constrained);
    assert_eq!(compositor.pointer_location(), (100.0, 100.0));

    let rect = compositor.focused_rect().unwrap();
    compositor.pointer_delta(f64::from(WIDTH) * 3.0, 0.0);
    let (x, _) = compositor.pointer_location();
    assert!(x < f64::from(rect.x + rect.w), "pointer left: {x}");

    connection.ungrab_pointer(0u32).unwrap();
    connection.flush().unwrap();
    wait_for(|| !constrained());
}

#[test]
fn a_dialog_of_x11_floats_and_may_move() {
    let Some((compositor, connection, root, _turn)) = with_x_server() else {
        return;
    };
    let main = x_window(&connection, root, false);
    wait_for(|| compositor.focused_rect().is_some());

    let dialog = connection.generate_id().unwrap();
    connection
        .create_window(
            COPY_DEPTH_FROM_PARENT,
            dialog,
            root,
            0,
            0,
            300,
            200,
            0,
            WindowClass::INPUT_OUTPUT,
            COPY_FROM_PARENT,
            &CreateWindowAux::new().background_pixel(0x0000ff00),
        )
        .unwrap();
    connection
        .change_property32(
            PropMode::REPLACE,
            dialog,
            AtomEnum::WM_TRANSIENT_FOR,
            AtomEnum::WINDOW,
            &[main],
        )
        .unwrap();
    connection.map_window(dialog).unwrap();
    connection.flush().unwrap();
    wait_for(|| compositor.frames().len() == 2);

    // In the middle of the output, as wide as it was made.
    wait_for(|| {
        let (x, y, w, h) = x_geometry(&connection, dialog);
        (w, h) == (300, 200) && (x, y) == ((WIDTH - 300) / 2, (HEIGHT - 200) / 2)
    });
    connection
        .configure_window(dialog, &ConfigureWindowAux::new().x(11).y(22).width(320))
        .unwrap();
    connection.flush().unwrap();
    wait_for(|| x_geometry(&connection, dialog) == (11, 22, 320, 200));
}

#[test]
fn a_dead_x_server_takes_its_windows_along_and_comes_back() {
    let Some((compositor, connection, root, _turn)) = with_x_server() else {
        return;
    };
    x_window(&connection, root, false);
    wait_for(|| compositor.focused_rect().is_some());

    kill_x_server(compositor.with(|state| state.xwayland.display).unwrap());
    wait_for(|| compositor.frames().is_empty());
    wait_for(|| compositor.with(|state| state.xwm.is_some()));
    let after = compositor.with(|state| state.xwayland.display).unwrap();
    let (connection, screen) = x11rb::connect(Some(&format!(":{after}"))).unwrap();
    let root = connection.setup().roots[screen].root;
    x_window(&connection, root, false);
    wait_for(|| compositor.focused_rect().is_some());
}

#[test]
fn screencopy_shows_what_is_on_the_output() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let image = screencopy(&compositor, &mut client, 0);
    let rgb = RED & 0xff_ffff;
    assert_eq!(captured_pixel(&image, WIDTH, WIDTH / 2, HEIGHT / 2), rgb);
    let shot = compositor.screenshot();
    for (x, y) in [(0, 0), (20, 20), (WIDTH - 1, HEIGHT - 1)] {
        assert_eq!(
            captured_pixel(&image, WIDTH, x, y),
            shot.at(x, y) & 0xff_ffff,
            "at {x},{y}"
        );
    }
}

#[test]
fn screencopy_of_a_region_is_that_part_and_stays_on_the_output() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_window(GREEN);
    let manager: ZwlrScreencopyManagerV1 = client.bind(1..=3);
    let output: WlOutput = client.bind(1..=4);

    // Straddles the line between the two windows, and sticks out at the bottom.
    let (x, y) = (WIDTH / 2 - 50, HEIGHT - 100);
    let frame = manager.capture_output_region(0, &output, x, y, 100, 300, &client.handle, ());
    let (width, height) = client.describe_frame();
    assert_eq!((width, height), (100, 100));
    let (buffer, file) = client.capture_buffer(width, height);
    client.state.copy = CopyState::default();
    frame.copy(&buffer);
    client.sync();
    compositor.run_captures(0);
    client.sync();
    assert!(client.state.copy.ready);
    let shot = compositor.screenshot();
    for (px, py) in [(5, 5), (95, 5), (50, 50), (5, 40)] {
        assert_eq!(
            captured_pixel(&file, width, px, py),
            shot.at(x + px, y + py) & 0xff_ffff,
            "at {px},{py}"
        );
    }

    client.state.copy = CopyState::default();
    let _outside = manager.capture_output_region(0, &output, WIDTH, 0, 10, 10, &client.handle, ());
    client.sync();
    assert!(client.state.copy.failed, "a region off the output");
}

#[test]
fn screencopy_draws_the_cursor_only_on_request() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    compositor.pointer_at(400.0, 300.0);
    client.sync();
    let pointer = client._pointer.clone();
    let cursor = client.compositor.create_surface(&client.handle, ());
    let _buffer = client.fill(&cursor, 8, 8, BLUE);
    pointer.set_cursor(client.state.enter_serial, Some(&cursor), 0, 0);
    client.sync();

    let with = screencopy(&compositor, &mut client, 1);
    assert_eq!(captured_pixel(&with, WIDTH, 403, 303), BLUE & 0xff_ffff);
    client.state.copy = CopyState::default();
    let without = screencopy(&compositor, &mut client, 0);
    assert_eq!(captured_pixel(&without, WIDTH, 403, 303), RED & 0xff_ffff);
}

#[test]
fn copy_with_damage_waits_for_a_change() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let window = client.open_window(RED);
    let manager: ZwlrScreencopyManagerV1 = client.bind(1..=3);
    let output: WlOutput = client.bind(1..=4);
    let (buffer, file) = client.capture_buffer(WIDTH, HEIGHT);

    let frame = manager.capture_output(0, &output, &client.handle, ());
    client.sync();
    frame.copy_with_damage(&buffer);
    client.sync();
    compositor.run_captures(0);
    client.sync();
    assert!(
        client.state.copy.ready,
        "the first frame has to come at once"
    );
    frame.destroy();

    client.state.copy = CopyState::default();
    let frame = manager.capture_output(0, &output, &client.handle, ());
    client.sync();
    frame.copy_with_damage(&buffer);
    client.sync();
    compositor.run_captures(0);
    compositor.run_captures(0);
    client.sync();
    assert!(!client.state.copy.ready, "nothing changed");

    client.draw(window, GREEN);
    compositor.run_captures(0);
    client.sync();
    assert!(client.state.copy.ready, "a change has to end the wait");
    assert_eq!(client.state.copy.damage, [(0, 0, WIDTH, HEIGHT)]);
    let rgb = GREEN & 0xff_ffff;
    assert_eq!(captured_pixel(&file, WIDTH, WIDTH / 2, HEIGHT / 2), rgb);
}

#[test]
fn a_capture_of_a_locked_session_shows_only_the_lock() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let output: WlOutput = client.bind(1..=4);
    let lock = client.lock();
    let _surface = client.lock_surface(&lock, &output, BLUE);
    let image = screencopy(&compositor, &mut client, 0);
    let rgb = BLUE & 0xff_ffff;
    assert_eq!(captured_pixel(&image, WIDTH, WIDTH / 2, HEIGHT / 2), rgb);
    assert_eq!(captured_pixel(&image, WIDTH, 30, 30), rgb);
}

#[test]
fn a_capture_of_an_output_that_goes_away_fails() {
    let compositor = Compositor::start();
    let second = compositor.add_output("second", (WIDTH, 0));
    let mut client = Client::connect(&compositor);
    let manager: ZwlrScreencopyManagerV1 = client.bind(1..=3);
    let globals = client.globals.contents().clone_list();
    let outputs = globals
        .iter()
        .filter(|global| global.interface == "wl_output");
    let names: Vec<u32> = outputs.map(|global| global.name).collect();
    let wl_output: WlOutput = client
        .globals
        .registry()
        .bind(names[1], 4, &client.handle, ());
    let frame = manager.capture_output(0, &wl_output, &client.handle, ());
    client.sync();
    let (buffer, _file) = client.capture_buffer(WIDTH, HEIGHT);
    frame.copy(&buffer);
    client.sync();
    compositor.with(move |state| state.remove_output(&second));
    client.sync();
    assert!(client.state.copy.failed);
}

#[test]
fn a_buffer_of_the_wrong_size_is_a_protocol_error() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let manager: ZwlrScreencopyManagerV1 = client.bind(1..=3);
    let output: WlOutput = client.bind(1..=4);
    let frame = manager.capture_output(0, &output, &client.handle, ());
    client.sync();
    let (buffer, _file) = client.capture_buffer(10, 10);
    frame.copy(&buffer);
    assert!(client.queue.roundtrip(&mut client.state).is_err());
    compositor.run_captures(0);
}

#[test]
fn image_copy_capture_gives_frames_as_the_output_changes() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let window = client.open_window(RED);
    let sources: ExtOutputImageCaptureSourceManagerV1 = client.bind(1..=1);
    let manager: ExtImageCopyCaptureManagerV1 = client.bind(1..=1);
    let output: WlOutput = client.bind(1..=4);
    let source = sources.create_source(&output, &client.handle, ());
    let session = manager.create_session(&source, Options::empty(), &client.handle, ());
    client.sync();
    assert!(client.state.copy.described);
    assert_eq!(client.state.copy.size, Some((WIDTH as u32, HEIGHT as u32)));
    assert_eq!(client.state.copy.formats, [Format::Xrgb8888 as u32]);

    let (buffer, file) = client.capture_buffer(WIDTH, HEIGHT);
    let frame = session.create_frame(&client.handle, ());
    frame.attach_buffer(&buffer);
    frame.damage_buffer(0, 0, WIDTH, HEIGHT);
    frame.capture();
    client.sync();
    compositor.run_captures(0);
    client.sync();
    assert!(
        client.state.copy.ready,
        "the first frame has to come at once"
    );
    let rgb = RED & 0xff_ffff;
    assert_eq!(captured_pixel(&file, WIDTH, WIDTH / 2, HEIGHT / 2), rgb);
    frame.destroy();

    client.state.copy.ready = false;
    let frame = session.create_frame(&client.handle, ());
    frame.attach_buffer(&buffer);
    frame.damage_buffer(0, 0, WIDTH, HEIGHT);
    frame.capture();
    client.sync();
    compositor.run_captures(0);
    client.sync();
    assert!(!client.state.copy.ready, "nothing changed");
    client.draw(window, GREEN);
    compositor.run_captures(0);
    client.sync();
    assert!(client.state.copy.ready, "a change has to end the wait");
    let rgb = GREEN & 0xff_ffff;
    assert_eq!(captured_pixel(&file, WIDTH, WIDTH / 2, HEIGHT / 2), rgb);
}

#[test]
fn image_copy_capture_learns_a_new_mode_and_ends_with_the_output() {
    let compositor = Compositor::start();
    let second = compositor.add_output("second", (WIDTH, 0));
    let mut client = Client::connect(&compositor);
    let sources: ExtOutputImageCaptureSourceManagerV1 = client.bind(1..=1);
    let manager: ExtImageCopyCaptureManagerV1 = client.bind(1..=1);
    let globals = client.globals.contents().clone_list();
    let outputs = globals
        .iter()
        .filter(|global| global.interface == "wl_output");
    let names: Vec<u32> = outputs.map(|global| global.name).collect();
    let wl_output: WlOutput = client
        .globals
        .registry()
        .bind(names[1], 4, &client.handle, ());
    let source = sources.create_source(&wl_output, &client.handle, ());
    let _session = manager.create_session(&source, Options::empty(), &client.handle, ());
    client.sync();

    let changed = second.clone();
    compositor.with(move |state| {
        let mode = Mode {
            size: (800, 500).into(),
            refresh: 60_000,
        };
        changed.change_current_state(Some(mode), None, None, None);
        state.output_resized(&changed);
    });
    client.sync();
    assert_eq!(client.state.copy.size, Some((800, 500)));

    compositor.with(move |state| state.remove_output(&second));
    client.sync();
    assert!(client.state.copy.stopped);
}

#[test]
fn window_lists_see_windows_come_change_and_go() {
    use zwlr_foreign_toplevel_handle_v1::State as ForeignState;
    let compositor = Compositor::start();
    let mut list = Client::connect(&compositor);
    let _manager = foreign_manager(&mut list);
    let _output: WlOutput = list.bind(1..=4);
    list.sync();

    let mut client = Client::connect(&compositor);
    let a = client.open_unmapped_window_with(|toplevel| {
        toplevel.set_title("first".into());
        toplevel.set_app_id("app.one".into());
    });
    list.sync();
    assert!(list.state.foreign.is_empty(), "listed before it shows");
    client.draw(a, RED);
    list.sync();
    assert_eq!(list.state.foreign.len(), 1);
    let entry = &list.state.foreign[0];
    assert_eq!(
        (entry.title.as_str(), entry.app_id.as_str()),
        ("first", "app.one")
    );
    assert_eq!(entry.states, [ForeignState::Activated as u32]);
    assert_eq!(entry.outputs, 1);
    assert_eq!(entry.done, 1);

    client.toplevel(a).set_title("renamed".into());
    client.sync();
    list.sync();
    assert_eq!(list.state.foreign[0].title, "renamed");
    assert_eq!(list.state.foreign[0].done, 2);

    let b = client.open_window(GREEN);
    list.sync();
    assert_eq!(list.state.foreign.len(), 2);
    assert_eq!(list.state.foreign[0].states, Vec::<u32>::new());
    assert_eq!(
        list.state.foreign[1].states,
        [ForeignState::Activated as u32]
    );

    client.close(b);
    list.sync();
    assert!(list.state.foreign[1].closed);
    assert!(!list.state.foreign[0].closed);
    list.state.foreign[1].handle.destroy();
    list.sync();
}

#[test]
fn a_late_window_list_gets_all_windows_and_its_requests_work() {
    use zwlr_foreign_toplevel_handle_v1::State as ForeignState;
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);

    let mut list = Client::connect(&compositor);
    let _output: WlOutput = list.bind(1..=4);
    list.sync();
    let _manager = foreign_manager(&mut list);
    compositor.with(|_| {});
    list.sync();
    assert_eq!(list.state.foreign.len(), 2);
    assert!(list.state.foreign.iter().all(|entry| entry.outputs == 1));
    assert!(list.state.foreign.iter().all(|entry| entry.done >= 1));

    let first = list.state.foreign[0].handle.clone();
    let seat = list.seat();
    first.activate(&seat);
    list.sync();
    client.sync();
    assert!(client.activated(a));
    assert!(!client.activated(b));
    list.sync();
    assert_eq!(
        list.state.foreign[0].states,
        [ForeignState::Activated as u32]
    );

    first.set_maximized();
    list.sync();
    client.sync();
    assert!(client.has_state(a, xdg_toplevel::State::Maximized));
    list.sync();
    assert!(
        list.state.foreign[0]
            .states
            .contains(&(ForeignState::Maximized as u32))
    );

    first.set_fullscreen(None);
    list.sync();
    client.sync();
    assert!(client.has_state(a, xdg_toplevel::State::Fullscreen));
    first.unset_fullscreen();
    list.sync();
    client.sync();
    assert!(!client.has_state(a, xdg_toplevel::State::Fullscreen));

    first.close();
    list.sync();
    client.sync();
    assert!(client.state.closed.contains(&a));
}

#[test]
fn a_window_on_another_workspace_stays_in_the_list_and_activating_brings_it_up() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let mut list = Client::connect(&compositor);
    let _manager = foreign_manager(&mut list);
    compositor.with(|_| {});
    list.sync();

    compositor.with(|state| state.show_workspace(1));
    let b = client.open_window(GREEN);
    list.sync();
    assert_eq!(list.state.foreign.len(), 2);
    let first = list.state.foreign[0].handle.clone();
    let seat = list.seat();
    first.activate(&seat);
    list.sync();
    assert_eq!(compositor.active_workspace(), 0);
    client.sync();
    assert!(!client.activated(b));
}

#[test]
fn a_parent_is_named_to_the_window_list() {
    let compositor = Compositor::start();
    let mut list = Client::connect(&compositor);
    let _manager = foreign_manager(&mut list);
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let parent = client.toplevel(a);
    let dialog = client.open_unmapped_window_with(|toplevel| toplevel.set_parent(Some(&parent)));
    client.draw_sized(dialog, (100, 100), GREEN);
    list.sync();
    assert_eq!(list.state.foreign.len(), 2);
    let child = list.state.foreign.iter().find(|entry| entry.parent);
    assert!(child.is_some(), "no parent event");
}

#[test]
fn a_clipboard_manager_reads_the_clipboard_without_the_keyboard() {
    let compositor = Compositor::start();
    let mut manager = Manager::connect(&compositor);
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let devices: WlDataDeviceManager = client.bind(1..=3);
    let seat = client.seat();
    let device = devices.get_data_device(&seat, &client.handle, ());
    let source = devices.create_data_source(&client.handle, ());
    source.offer("text/plain".into());
    compositor.tap_key(KEY_A);
    client.sync();
    device.set_selection(Some(&source), client.state.key_serial);
    client.sync();

    manager.client.sync();
    let offer = manager
        .client
        .state
        .control_selection
        .clone()
        .expect("the manager saw no selection");
    let types = manager.client.state.control_offers.iter();
    let types = types.into_iter().find(|(other, _)| *other == offer);
    assert_eq!(types.unwrap().1, ["text/plain"]);
    let text = exchange(&mut client, &mut manager.client, |_, fd| {
        offer.receive("text/plain".into(), fd);
    });
    assert_eq!(text, CLIPBOARD_TEXT);
}

#[test]
fn a_clipboard_manager_sets_the_clipboard_for_the_window_with_the_keyboard() {
    let compositor = Compositor::start();
    let mut manager = Manager::connect(&compositor);
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let devices: WlDataDeviceManager = client.bind(1..=3);
    let seat = client.seat();
    let _device = devices.get_data_device(&seat, &client.handle, ());
    client.sync();

    let source = manager
        .control
        .create_data_source(&manager.client.handle, ());
    source.offer("text/plain".into());
    manager._device.set_selection(Some(&source));
    manager.client.sync();
    client.sync();
    let offer = client.state.selection.clone().expect("no offer");
    let text = exchange(&mut manager.client, &mut client, |_, fd| {
        offer.receive("text/plain".into(), fd);
    });
    assert_eq!(text, CONTROL_TEXT);
}

#[test]
fn a_clipboard_manager_also_gets_the_primary_selection() {
    let compositor = Compositor::start();
    let mut manager = Manager::connect(&compositor);
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let devices: ZwpPrimarySelectionDeviceManagerV1 = client.bind(1..=1);
    let seat = client.seat();
    let device = devices.get_device(&seat, &client.handle, ());
    let source = devices.create_source(&client.handle, ());
    source.offer("text/plain".into());
    compositor.tap_key(KEY_A);
    client.sync();
    device.set_selection(Some(&source), client.state.key_serial);
    client.sync();
    manager.client.sync();
    assert!(manager.client.state.control_primary.is_some());
}

#[test]
fn a_clipboard_manager_is_cancelled_when_a_window_copies() {
    let compositor = Compositor::start();
    let mut manager = Manager::connect(&compositor);
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let source = manager
        .control
        .create_data_source(&manager.client.handle, ());
    source.offer("text/plain".into());
    manager._device.set_selection(Some(&source));
    manager.client.sync();

    let devices: WlDataDeviceManager = client.bind(1..=3);
    let seat = client.seat();
    let device = devices.get_data_device(&seat, &client.handle, ());
    let other = devices.create_data_source(&client.handle, ());
    other.offer("text/plain".into());
    compositor.tap_key(KEY_A);
    client.sync();
    device.set_selection(Some(&other), client.state.key_serial);
    client.sync();
    manager.client.sync();
    assert!(manager.client.state.control_cancelled);
}

/// The way Chromium moves a bubble when its window gets another size: `set_position` and a commit
/// of the parent alone. The bubble must be seen and clicked in the new place.
#[test]
fn a_subsurface_moves_with_the_commit_of_its_parent() {
    for desync in [true, false] {
        let compositor = Compositor::start();
        let mut client = Client::connect(&compositor);
        let window = client.open_window(BLUE);
        let parent = client.state.windows[window].surface.clone();
        let subcompositor: WlSubcompositor = client.bind(1..=1);
        let child = client.compositor.create_surface(&client.handle, ());
        let subsurface = subcompositor.get_subsurface(&child, &parent, &client.handle, ());
        if desync {
            subsurface.set_desync();
        }
        let id = client.track(&child);
        subsurface.set_position(50, 50);
        let _buffer = client.fill(&child, 100, 100, RED);
        parent.commit();
        client.sync();
        let frame = compositor.focused_rect().unwrap();
        let (old, new) = ((frame.x + 60, frame.y + 60), (frame.x + 310, frame.y + 60));
        let shot = compositor.screenshot();
        assert_eq!(shot.at(old.0, old.1), RED);
        assert_eq!(shot.at(new.0, new.1), BLUE);

        subsurface.set_position(300, 50);
        client.sync();
        assert_eq!(compositor.screenshot().at(old.0, old.1), RED, "too early");

        parent.commit();
        client.sync();
        let shot = compositor.screenshot();
        assert_eq!(shot.at(old.0, old.1), BLUE, "desync: {desync}");
        assert_eq!(shot.at(new.0, new.1), RED, "desync: {desync}");

        compositor.pointer_at(f64::from(new.0), f64::from(new.1));
        client.sync();
        let entered = client
            .take_input()
            .into_iter()
            .any(|input| matches!(input, Input::PointerEnter(index, ..) if index == id));
        assert!(entered, "desync: {desync}");
    }
}

const KEY_RIGHTALT: u32 = 100; // AltGr on a German keyboard

#[test]
fn a_modifier_key_can_be_bound() {
    let compositor = Compositor::start_with("bind = CTRL, Shift_R, workspace, 3\n");
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.take_input();

    compositor.chord(&[KEY_LEFTSHIFT], KEY_RIGHTSHIFT);
    assert_eq!(compositor.active_workspace(), 0, "Shift instead of Ctrl");
    compositor.tap_key(KEY_RIGHTSHIFT);
    assert_eq!(compositor.active_workspace(), 0, "no Ctrl held");

    compositor.key(KEY_LEFTCTRL, KeyState::Pressed);
    assert_eq!(compositor.active_workspace(), 0, "Ctrl alone");
    compositor.tap_key(KEY_RIGHTSHIFT);
    compositor.key(KEY_LEFTCTRL, KeyState::Released);
    assert_eq!(compositor.active_workspace(), 2);
}

#[test]
fn clients_see_a_bound_modifier_key_go_down_and_up() {
    let compositor = Compositor::start_with("bind = CTRL, Shift_R, focus_next\n");
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.take_input();
    compositor.chord(&[KEY_LEFTCTRL], KEY_RIGHTSHIFT);
    let input = client.take_input();
    for wanted in [
        Input::Key(KEY_RIGHTSHIFT, wl_keyboard::KeyState::Pressed),
        Input::Key(KEY_RIGHTSHIFT, wl_keyboard::KeyState::Released),
    ] {
        assert!(input.contains(&wanted), "{input:?}");
    }
}

#[test]
fn the_wheel_can_be_bound() {
    let compositor = Compositor::start_with(
        "axis_bind_apply_timeout = 0
         axisbind = SUPER, UP, workspace, 2
         axisbind = SUPER, DOWN, workspace, 3
         axisbind = SUPER, LEFT, workspace, 4
         axisbind = SUPER, RIGHT, workspace, 5\n",
    );
    let scroll = |horizontal: f64, vertical: f64| {
        compositor.with(move |state| {
            let mut scroll = Scroll::new(AxisSource::Wheel, InputTime::now());
            scroll.set(Axis::Horizontal, Some(horizontal), None);
            scroll.set(Axis::Vertical, Some(vertical), Some(vertical * 8.0));
            state.on_pointer_axis(scroll);
        });
    };
    scroll(0.0, -15.0);
    assert_eq!(compositor.active_workspace(), 0, "no modifier held");
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    for (horizontal, vertical, workspace) in [
        (0.0, -15.0, 1),
        (0.0, 15.0, 2),
        (-15.0, 0.0, 3),
        (15.0, 0.0, 4),
    ] {
        scroll(horizontal, vertical);
        assert_eq!(compositor.active_workspace(), workspace);
    }
}

#[test]
fn a_bound_wheel_does_not_scroll_and_acts_once_per_timeout() {
    let compositor = Compositor::start_with(
        "axis_bind_apply_timeout = 60000
         axisbind = SUPER, DOWN, workspace, next\n",
    );
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    client.draw(a, RED);
    compositor.pointer_at(100.0, 100.0);
    client.take_input();
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    for _ in 0..5 {
        compositor.with(|state| {
            let mut scroll = Scroll::new(AxisSource::Wheel, InputTime::now());
            scroll.set(Axis::Vertical, Some(15.0), Some(120.0));
            state.on_pointer_axis(scroll);
        });
    }
    assert_eq!(compositor.active_workspace(), 1);
    let input = client.take_input();
    assert!(
        !input.iter().any(|event| matches!(event, Input::Axis(..))),
        "{input:?}"
    );
}

#[test]
fn a_locked_session_has_no_axis_binds() {
    let compositor = Compositor::start_with("axisbind = SUPER, DOWN, workspace, next\n");
    let mut locker = Client::connect(&compositor);
    let _lock = locker.lock();
    locker.sync();
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.with(|state| {
        let mut scroll = Scroll::new(AxisSource::Wheel, InputTime::now());
        scroll.set(Axis::Vertical, Some(15.0), Some(120.0));
        state.on_pointer_axis(scroll);
    });
    assert_eq!(compositor.active_workspace(), 0);
}

#[test]
fn used_workspaces_skip_the_empty_ones_and_stop_at_the_ends() {
    let compositor = Compositor::start_with(
        "bind = SUPER, Left, workspace, prev_used
         bind = SUPER, Right, workspace, next_used
         bind = SUPER, 4, workspace, 4
         bind = SUPER+SHIFT, 7, move_to_workspace, 7\n",
    );
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_window(GREEN);
    compositor.chord(&[KEY_LEFTMETA, KEY_LEFTSHIFT], 8); // 7
    assert_eq!(compositor.active_workspace(), 6);
    compositor.chord(&[KEY_LEFTMETA], KEY_LEFT);
    assert_eq!(compositor.active_workspace(), 0);
    compositor.chord(&[KEY_LEFTMETA], KEY_LEFT);
    assert_eq!(compositor.active_workspace(), 0, "no wrapping");
    compositor.chord(&[KEY_LEFTMETA], KEY_RIGHT);
    assert_eq!(compositor.active_workspace(), 6);
    compositor.chord(&[KEY_LEFTMETA], KEY_RIGHT);
    assert_eq!(compositor.active_workspace(), 6, "no wrapping");
}

#[test]
fn a_held_bind_repeats_until_its_key_goes_up() {
    let compositor = Compositor::start_with(REPEATING);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.key(KEY_RIGHT, KeyState::Pressed);
    assert_eq!(compositor.active_workspace(), 1, "at once on the press");
    wait_for(|| compositor.active_workspace() >= 3);
    compositor.key(KEY_RIGHT, KeyState::Released);
    let stopped = compositor.active_workspace();
    thread::sleep(Duration::from_millis(60));
    assert_eq!(compositor.active_workspace(), stopped);
    assert!(!compositor.with(|state| state.bind_repeat.is_some()));
}

#[test]
fn releasing_the_modifier_ends_the_repeat_as_well() {
    let compositor = Compositor::start_with(REPEATING);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.key(KEY_RIGHT, KeyState::Pressed);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    thread::sleep(Duration::from_millis(90));
    assert_eq!(compositor.active_workspace(), 1);
    compositor.key(KEY_RIGHT, KeyState::Released);
}

#[test]
fn the_repeat_waits_for_the_delay() {
    let compositor = Compositor::start_with(
        "repeat_delay = 60000
         bind = SUPER, Right, workspace, next\n",
    );
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.key(KEY_RIGHT, KeyState::Pressed);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(compositor.active_workspace(), 1);
}

#[test]
fn holding_a_bind_that_is_no_step_runs_it_once() {
    let compositor = Compositor::start_with(REPEATING);
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_window(GREEN);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.key(KEY_W, KeyState::Pressed);
    assert!(!compositor.with(|state| state.bind_repeat.is_some()));
    thread::sleep(Duration::from_millis(70));
    client.sync();
    assert_eq!(client.state.closed.len(), 1);
}

#[test]
fn a_lock_ends_the_repeat() {
    let compositor = Compositor::start_with(REPEATING);
    let mut locker = Client::connect(&compositor);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.key(KEY_RIGHT, KeyState::Pressed);
    let _lock = locker.lock();
    locker.sync();
    let locked = compositor.active_workspace();
    thread::sleep(Duration::from_millis(70));
    assert_eq!(compositor.active_workspace(), locked);
}

#[test]
fn altgr_reaches_the_client_and_fires_no_bind() {
    // ALT, q closes the window by default. AltGr+Q is the @ of a German keyboard.
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.take_input();
    compositor.chord(&[KEY_RIGHTALT], KEY_Q);
    let input = client.take_input();
    for wanted in [
        Input::Key(KEY_RIGHTALT, wl_keyboard::KeyState::Pressed),
        Input::Key(KEY_Q, wl_keyboard::KeyState::Pressed),
        Input::Key(KEY_Q, wl_keyboard::KeyState::Released),
        Input::Key(KEY_RIGHTALT, wl_keyboard::KeyState::Released),
    ] {
        assert!(input.contains(&wanted), "{input:?}");
    }
    client.sync();
    assert!(client.state.closed.is_empty());
}

#[test]
fn altgr_keeps_binds_of_the_plain_key_out_of_it() {
    let compositor = Compositor::start_with("bind = SUPER, 2, workspace, 2\n");
    compositor.chord(&[KEY_LEFTMETA, KEY_RIGHTALT], KEY_2);
    assert_eq!(compositor.active_workspace(), 0);
}

#[test]
fn altgr_can_be_part_of_a_bind() {
    let compositor = Compositor::start_with(
        "bind = ALTGR, q, workspace, 2
         bind = SUPER+ALTGR, a, workspace, 3\n",
    );
    compositor.chord(&[KEY_RIGHTALT], KEY_Q);
    assert_eq!(compositor.active_workspace(), 1);
    compositor.chord(&[KEY_LEFTMETA, KEY_RIGHTALT], KEY_A);
    assert_eq!(compositor.active_workspace(), 2);
}

#[test]
fn a_symbol_behind_altgr_can_be_bound_by_that_modifier() {
    let compositor = Compositor::start_with("bind = ALTGR, at, workspace, 2\n");
    compositor.chord(&[KEY_RIGHTALT], KEY_Q);
    assert_eq!(compositor.active_workspace(), 1);
}

#[test]
fn exec_once_keeps_its_command_as_written() {
    let (config, errors) = Config::parse("exec-once=touch a b, c=d\nexec-once = x &\n");
    assert_eq!(errors, []);
    assert_eq!(config.exec_once, ["touch a b, c=d", "x &"]);
    let (_, errors) = Config::parse("exec-once =\n");
    assert_eq!(errors.len(), 1);
}

#[test]
fn mango_lines_for_binds_work_as_they_are() {
    let (config, errors) = Config::parse(
        "bind=CTRL,Shift_R,spawn,qs -p /a/menu ipc call wizard blob
         axisbind=SUPER,UP,workspace,prev_used\n",
    );
    assert_eq!(errors, []);
    assert_eq!(config.binds.len(), 2, "plus the quit bind");
    assert_eq!(config.axis_binds.len(), 1);
}

#[test]
fn bars_can_follow_the_workspaces() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let mut tags = ipc_lines(&compositor, "watch-tags");
    let first = next_line(&mut tags);
    assert!(
        first.contains(
            r#""index":1,"is_active":true,"is_urgent":false,"layout":"DW","client_count":0"#
        ),
        "{first}"
    );

    client.open_window(RED);
    let opened = next_line(&mut tags);
    assert!(
        opened.contains(
            r#""index":1,"is_active":true,"is_urgent":false,"layout":"DW","client_count":1"#
        ),
        "{opened}"
    );

    let mut answer = ipc_lines(&compositor, "workspace 3");
    assert_eq!(next_line(&mut answer), "ok\n");
    let moved = next_line(&mut tags);
    assert!(moved.contains(r#""index":3,"is_active":true"#), "{moved}");
    assert!(
        moved.contains(
            r#""index":1,"is_active":false,"is_urgent":false,"layout":"DW","client_count":1"#
        ),
        "{moved}"
    );
}

#[test]
fn bars_can_ask_for_the_monitors() {
    let compositor = Compositor::start();
    let mut monitors = ipc_lines(&compositor, "monitors");
    let line = next_line(&mut monitors);
    assert!(
        line.starts_with(r#"{"monitors":[{"name":"test","active":true,"x":0,"y":0,"#),
        "{line}"
    );
}

#[test]
fn a_bar_can_end_the_session() {
    let mut compositor = Compositor::start();
    let mut quit = ipc_lines(&compositor, "quit");
    assert_eq!(next_line(&mut quit), "ok\n");
    compositor.thread.take().unwrap().join().unwrap();
}

// Session 10: hardening.

#[test]
fn altgr_e_reaches_the_client_as_altgr_and_e() {
    // The euro sign of a German keyboard. No bind may take it, the session template least of all.
    let compositor = Compositor::start_with(include_str!("../session/config"));
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.take_input();
    const KEY_E: u32 = 18;
    compositor.chord(&[KEY_RIGHTALT], KEY_E);
    let input = client.take_input();
    for wanted in [
        Input::Key(KEY_E, wl_keyboard::KeyState::Pressed),
        Input::Key(KEY_E, wl_keyboard::KeyState::Released),
    ] {
        assert!(input.contains(&wanted), "{input:?}");
    }
}

#[test]
fn the_offset_of_a_cursor_surface_moves_its_hotspot() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    compositor.pointer_at(400.0, 300.0);
    client.sync();
    let pointer = client._pointer.clone();
    let cursor = client.compositor.create_surface(&client.handle, ());
    let _buffer = client.fill(&cursor, 8, 8, BLUE);
    pointer.set_cursor(client.state.enter_serial, Some(&cursor), 4, 4);
    client.sync();
    assert_eq!(compositor.screenshot().at(396, 296), BLUE);

    cursor.offset(-3, -2);
    cursor.commit();
    client.sync();
    // The hotspot stays in the picture where the client's image moved to.
    assert_eq!(compositor.screenshot().at(399, 298), BLUE);
    assert_ne!(compositor.screenshot().at(392, 294), BLUE);
}

#[test]
fn a_tile_that_stays_too_big_ends_at_its_place() {
    let (compositor, _client, _) = overflowing_tile();
    let shot = compositor.screenshot();
    assert_eq!(shot.at(WIDTH / 2, HEIGHT / 2), RED);
    assert_ne!(shot.at(WIDTH - 20, HEIGHT / 2), RED);
    assert_ne!(shot.at(WIDTH / 2, HEIGHT - 20), RED);
}

#[test]
fn what_is_cut_off_gets_no_pointer_events() {
    let (compositor, mut client, _) = overflowing_tile();
    compositor.pointer_at(f64::from(WIDTH - 20), f64::from(HEIGHT / 2));
    client.sync();
    assert_eq!(client.take_input(), []);
}

#[test]
fn a_tile_that_stays_too_big_keeps_its_popups() {
    let (compositor, mut client, tile) = overflowing_tile();
    let parent = client.xdg_surface(tile);
    let _popup = client.open_popup(&parent, (WIDTH - 90, 100), (40, 40), POPUP, None);
    client.sync();
    assert_eq!(compositor.screenshot().at(WIDTH - 35, 160), POPUP);
}

#[test]
fn a_window_goes_fullscreen_on_the_output_it_names() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    compositor.add_output("second", (WIDTH, 0));
    let a = client.open_window(RED);
    client.sync();
    let output = client
        .globals
        .contents()
        .clone_list()
        .iter()
        .filter(|global| global.interface == "wl_output")
        .map(|global| {
            let registry = client.globals.registry();
            registry.bind::<WlOutput, _, _>(global.name, 4, &client.handle, ())
        })
        .nth(1)
        .unwrap();
    client.sync();
    client.toplevel(a).set_fullscreen(Some(&output));
    client.sync();
    let (monitor, area) = compositor.with(|state| {
        let window = state.focused.clone().unwrap();
        let (monitor, _) = state.locate(&window).unwrap();
        (monitor, state.monitor_area(monitor).unwrap())
    });
    assert_eq!(monitor, 1);
    assert_eq!(client.size(a), (area.w, area.h));
    assert!(client.has_state(a, xdg_toplevel::State::Fullscreen));
}

#[test]
fn a_popup_grab_with_an_old_serial_is_refused() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    compositor.pointer_at(100.0, 100.0);
    compositor.click();
    client.sync();
    let old = client.state.button_serial;
    compositor.click();
    client.sync();
    let parent = client.xdg_surface(a);
    let _popup = client.open_popup(&parent, (200, 200), (100, 100), POPUP, Some(old));
    client.sync();
    assert_eq!(client.state.popups_done, 1);
    assert!(compositor.with(|state| state.popup_grab.is_none()));
}

#[test]
fn a_window_that_wants_attention_shows_in_the_tags_until_it_has_the_focus() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let far = client.open_window(RED);
    let near = client.open_window(GREEN);
    client.sync();
    let surface = client.state.windows[far].surface.clone();
    let token = activation_token(&mut client, false, &surface);
    let activation = client.bind::<XdgActivationV1>(1..=1);
    activation.activate(token, &client.state.windows[far].surface);
    client.sync();
    client.sync();
    assert!(client.state.windows[near].activated);
    assert!(next_line(&mut ipc_lines(&compositor, "tags")).contains(r#""is_urgent":true"#));

    compositor.with(|state| {
        let window = state.urgent[0].clone();
        state.set_focus(Some(window));
    });
    assert!(!next_line(&mut ipc_lines(&compositor, "tags")).contains(r#""is_urgent":true"#));
}

#[test]
fn absurd_sizes_do_no_harm() {
    let compositor = Compositor::start();
    for geometry in [
        (0, 0, 1, 1),
        (i32::MAX, i32::MAX, i32::MAX, i32::MAX),
        (i32::MIN, i32::MIN, i32::MAX, i32::MAX),
        (0, 0, i32::MAX, 1),
    ] {
        let mut client = Client::connect(&compositor);
        let a = client.open_window(RED);
        let (x, y, w, h) = geometry;
        client.xdg_surface(a).set_window_geometry(x, y, w, h);
        client.state.windows[a].surface.commit();
        client.sync_or_die();
        compositor.screenshot();
        compositor.pointer_at(10.0, 10.0);
        survives(&compositor);
    }
    let mut client = Client::connect(&compositor);
    let a = client.open_unmapped_window_with(|toplevel| {
        toplevel.set_min_size(i32::MAX, i32::MAX);
        toplevel.set_max_size(1, 1);
    });
    client.draw(a, RED);
    client.sync_or_die();
    compositor.screenshot();
    survives(&compositor);
}

#[test]
fn a_subsurface_far_away_does_no_harm() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let window = client.open_window(RED);
    let subcompositor: WlSubcompositor = client.bind(1..=1);
    let parent = client.state.windows[window].surface.clone();
    let child = client.compositor.create_surface(&client.handle, ());
    let subsurface = subcompositor.get_subsurface(&child, &parent, &client.handle, ());
    subsurface.set_desync();
    let _buffer = client.fill(&child, 10, 10, GREEN);
    for position in [i32::MAX / 2, -i32::MAX / 2, 100_000] {
        subsurface.set_position(position, position);
        parent.commit();
        client.sync_or_die();
        compositor.screenshot();
    }
    survives(&compositor);
}

#[test]
fn a_cursor_with_absurd_numbers_does_no_harm() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    compositor.pointer_at(100.0, 100.0);
    client.sync();
    let pointer = client._pointer.clone();
    let cursor = client.compositor.create_surface(&client.handle, ());
    let _buffer = client.fill(&cursor, 8, 8, BLUE);
    pointer.set_cursor(client.state.enter_serial, Some(&cursor), i32::MAX, i32::MIN);
    client.sync();
    compositor.screenshot();
    for _ in 0..3 {
        cursor.offset(i32::MAX, i32::MIN);
        cursor.commit();
        client.sync();
    }
    compositor.screenshot();
    cursor.destroy();
    client.sync_or_die();
    compositor.screenshot();
    survives(&compositor);
}

#[test]
fn a_client_may_leave_in_the_middle_of_anything() {
    let compositor = Compositor::start();
    // In a popup grab.
    {
        let (_, _) = {
            let mut client = Client::connect(&compositor);
            let a = client.open_window(RED);
            compositor.pointer_at(100.0, 100.0);
            compositor.button(BTN_LEFT, ButtonState::Pressed);
            client.sync();
            let serial = client.state.button_serial;
            let parent = client.xdg_surface(a);
            let _popup = client.open_popup(&parent, (20, 20), (50, 50), POPUP, Some(serial));
            (a, serial)
        };
        compositor.button(BTN_LEFT, ButtonState::Released);
        survives(&compositor);
    }
    // In a drag of its own window.
    {
        let mut client = Client::connect(&compositor);
        client.open_window(RED);
        compositor.pointer_at(100.0, 100.0);
        compositor.button(BTN_LEFT, ButtonState::Pressed);
        client.sync();
        let seat = client.seat();
        let serial = client.state.button_serial;
        client.toplevel(0)._move(&seat, serial);
        client.sync();
        drop(client);
        compositor.pointer_at(300.0, 200.0);
        compositor.button(BTN_LEFT, ButtonState::Released);
        survives(&compositor);
    }
    compositor.with(|state| assert!(state.popup_grab.is_none()));
}

#[test]
fn a_popup_of_a_gone_parent_does_no_harm() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let parent = client.xdg_surface(a);
    let popup = client.open_popup(&parent, (20, 20), (50, 50), POPUP, None);
    let nested = client.open_popup(&popup.xdg_surface, (5, 5), (20, 20), POPUP, None);
    // Out of order, which the protocol forbids.
    popup.popup.destroy();
    client.sync_or_die();
    nested.popup.destroy();
    client.sync_or_die();
    compositor.screenshot();
    survives(&compositor);
}

#[test]
fn a_hundred_windows_fit_somehow() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    for index in 0..100 {
        let window = client.open_unmapped_window();
        let size = client.size(window);
        assert!(size.0 > 0 && size.1 > 0, "window {index} is {size:?}");
        client.draw(window, RED);
    }
    compositor.screenshot();
    for frame in compositor.frames() {
        assert!(frame.w > 0 && frame.h > 0, "{frame:?}");
    }
    drop(client);
    compositor.wait_for_clients(0);
    survives(&compositor);
    assert!(compositor.frames().is_empty());
}

#[test]
fn nothing_stays_behind_when_windows_and_clients_go() {
    let compositor = Compositor::start();
    for round in 0..30 {
        let mut client = Client::connect(&compositor);
        let a = client.open_window(RED);
        let b = client.open_window(GREEN);
        let parent = client.xdg_surface(a);
        let _popup = client.open_popup(&parent, (20, 20), (50, 50), POPUP, None);
        let surface = client.state.windows[a].surface.clone();
        let token = activation_token(&mut client, false, &surface);
        let activation = client.bind::<XdgActivationV1>(1..=1);
        activation.activate(token, &client.state.windows[a].surface);
        client.sync();
        client.close(b);
        client.sync();
        // Half of them end without saying goodbye.
        if round < 15 {
            client.close(a);
            client.sync();
        }
    }
    compositor.wait_for_clients(0);
    survives(&compositor);
    compositor.with(|state| {
        state.popups.cleanup();
        assert!(state.urgent.is_empty());
        assert!(state.awaiting_buffer.is_empty());
        assert!(state.unconfigured.is_empty());
        assert!(state.frames.is_empty());
        assert!(state.borders.is_empty());
        assert_eq!(state.space.elements().count(), 0);
    });
}

#[test]
fn plugins_list_windows_as_json() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let mut ipc = compositor.raw_ipc();
    assert_eq!(ipc.request("windows"), "[]");
    client.open_window(0x00ff_0000);
    let id = first_window_id(&mut ipc);
    assert_eq!(
        ipc.request("windows"),
        format!(
            r#"[{{"id":{id},"app_id":"","title":"","output":"test","workspace":1,"focused":true,"floating":false,"visible":true,"x":14,"y":14,"width":972,"height":572}}]"#
        )
    );
}

#[test]
fn subscribers_hear_about_windows_focus_and_workspaces() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let mut events = compositor.raw_ipc();
    assert_eq!(events.request("subscribe"), "ok");

    client.open_window(0x00ff_0000);
    let mut ipc = compositor.raw_ipc();
    let id = first_window_id(&mut ipc);
    assert_eq!(events.line(), format!("event open {id} -"));
    assert_eq!(events.line(), format!("event focus {id}"));

    compositor.run(&["ws", "2"]);
    assert_eq!(events.line(), "event focus none");
    assert_eq!(events.line(), "event workspace test 2");
    compositor.run(&["ws", "1"]);
    assert_eq!(events.line(), format!("event focus {id}"));
    assert_eq!(events.line(), "event workspace test 1");

    client.sync();
    client.close(0);
    assert_eq!(events.line(), format!("event close {id}"));
    assert_eq!(events.line(), "event focus none");
}

#[test]
fn effects_move_fade_and_colour_a_window_until_the_plugin_leaves() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(0x00ff_0000);
    let background = rgb(Config::default().background_color);
    let focus = rgb(Config::default().focus_color);
    let shot = compositor.screenshot();
    assert_eq!(shot.at(14, 300), 0xff_0000);
    assert_eq!(shot.at(11, 300), focus);

    let mut plugin = compositor.raw_ipc();
    let id = first_window_id(&mut plugin);
    assert_eq!(plugin.request(&format!("offset {id} 100 0")), "ok");
    let shot = compositor.screenshot();
    assert_eq!(shot.at(14, 300), background, "the window moved away");
    assert_eq!(shot.at(111, 300), focus, "its border moved along");
    assert_eq!(shot.at(114, 300), 0xff_0000, "and is drawn further right");

    assert_eq!(plugin.request("border focused 0x00ff00"), "ok");
    assert_eq!(compositor.screenshot().at(111, 300), 0x00_ff00);
    // Only the border shows it here: pixman draws a buffer without alpha with `Src`, so a window
    // of the test client turns black instead of see-through. GL blends it.
    assert_eq!(plugin.request(&format!("opacity {id} 0")), "ok");
    assert_eq!(compositor.screenshot().at(111, 300), background);
    let opacity = compositor.with(|state| {
        let window = state.focused.clone().unwrap();
        state.plugin_state.effect(&window).unwrap().opacity
    });
    assert_eq!(opacity, 0.0);

    // The input stays where the layout put it.
    compositor.pointer_at(50.0, 300.0);
    assert!(compositor.with(|state| state.window_at((50.0, 300.0).into()).is_some()));

    assert_eq!(plugin.request("offset 999 1 1"), "error no window 999");
    assert_eq!(plugin.request(&format!("reset {id}")), "ok");
    assert_eq!(compositor.screenshot().at(14, 300), 0xff_0000);

    assert_eq!(plugin.request(&format!("offset {id} 0 100")), "ok");
    assert_eq!(compositor.screenshot().at(300, 50), background);
    drop(plugin);
    wait_until("the effects to go with their connection", || {
        compositor.screenshot().at(300, 50) == 0xff_0000
    });
}

#[test]
fn effects_of_two_plugins_add_up() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(0x00ff_0000);
    let (mut one, mut two) = (compositor.raw_ipc(), compositor.raw_ipc());
    assert_eq!(one.request("offset focused 50 0"), "ok");
    assert_eq!(two.request("offset focused 50 0"), "ok");
    let shot = compositor.screenshot();
    assert_eq!(shot.at(109, 300), rgb(Config::default().background_color));
    assert_eq!(shot.at(110, 300), rgb(Config::default().focus_color));
    assert_eq!(shot.at(114, 300), 0xff_0000);
    assert_eq!(two.request("reset all"), "ok");
    assert_eq!(compositor.screenshot().at(64, 300), 0xff_0000);
}

#[test]
fn plugins_dispatch_bind_actions() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(0x00ff_0000);
    let mut ipc = compositor.raw_ipc();
    assert_eq!(ipc.request("dispatch move_to_workspace 3"), "ok");
    assert_eq!(
        compositor.with(|state| state.monitors[0].workspaces[2].windows().len()),
        1
    );
    assert_eq!(ipc.request("dispatch workspace 3"), "ok");
    assert_eq!(compositor.active_workspace(), 2);
    assert_eq!(
        ipc.request("dispatch explode"),
        "error unknown action `explode`"
    );
    assert_eq!(
        ipc.request("opacity 1 2"),
        "error opacity goes from 0 to 1, got 2"
    );
}

#[test]
fn wirbel_plug_does_several_plugins_at_once_or_none() {
    use std::os::unix::fs::PermissionsExt;
    let setup = PlugSetup::new("plug-several");
    let compositor = Compositor::start();
    let second = setup.folder.join("zwei.sh");
    fs::write(&second, "#!/bin/sh\nexec sleep 60\n").unwrap();
    fs::set_permissions(&second, fs::Permissions::from_mode(0o755)).unwrap();
    let (one, two) = (setup.plugin.to_str().unwrap(), second.to_str().unwrap());

    assert_eq!(
        setup.plug(&compositor, &["add", one, "eins", two]),
        format!("added eins ({one}), on\nadded {two}, on\n")
    );
    let before = fs::read_to_string(&setup.config).unwrap();
    assert!(before.contains(&format!(
        "plugin = {one}, eins, true\nplugin = {two}, true\n"
    )));

    assert_eq!(
        setup.plug(&compositor, &["eins", two, "false"]),
        format!("eins is off\n{two} is off\n")
    );
    let off = fs::read_to_string(&setup.config).unwrap();
    assert!(off.contains(&format!(
        "plugin = {one}, eins, false\nplugin = {two}, false\n"
    )));

    // One unknown plugin, and nothing changes.
    assert!(
        setup
            .plug_failing(&compositor, &["eins", "nichts", "true"])
            .contains("no plugin nichts")
    );
    assert_eq!(fs::read_to_string(&setup.config).unwrap(), off);

    assert_eq!(
        setup.plug(&compositor, &["remove", "eins", two]),
        format!("removed eins\nremoved {two}\n")
    );
    assert_eq!(
        fs::read_to_string(&setup.config).unwrap(),
        "# mine\nsloppy_focus = 0\n"
    );
}

#[test]
fn wirbel_plug_edits_the_config_and_starts_and_stops_plugins() {
    let setup = PlugSetup::new("plug");
    let compositor = Compositor::start();
    let config = setup.config.clone();
    compositor.with(move |state| state.config_path = Some(config));
    let path = setup.plugin.to_str().unwrap();

    assert!(setup.plug(&compositor, &["list"]).starts_with("no plugins"));
    assert_eq!(
        setup.plug(&compositor, &["add", path]),
        format!("added {path}, on\n")
    );
    wait_until("the plugin to start", || setup.marker.exists());
    assert!(setup.running(&compositor));
    let ran = fs::read_to_string(&setup.marker).unwrap();
    assert!(ran.starts_with(&format!("{path} ")), "{ran}");
    assert!(ran.trim_end().ends_with(".sock"), "{ran}");
    assert_eq!(
        setup.plug(&compositor, &["list"]),
        format!("on   {path}  running\n")
    );

    setup.plug(&compositor, &["define", path, "marker"]);
    assert_eq!(
        setup.plug(&compositor, &["list"]),
        format!("on   marker  {path}  running\n")
    );
    assert!(
        setup
            .plug_failing(&compositor, &["add", path])
            .contains("added already")
    );
    assert!(
        setup
            .plug_failing(&compositor, &["define", "marker", "true"])
            .contains("cannot be a name")
    );
    assert!(
        setup
            .plug_failing(&compositor, &["nothing", "true"])
            .contains("no plugin nothing")
    );

    assert_eq!(
        setup.plug(&compositor, &["marker", "false"]),
        "marker is off\n"
    );
    wait_until("the plugin to stop", || !setup.running(&compositor));
    assert_eq!(
        setup.plug(&compositor, &["list"]),
        format!("off  marker  {path}\n")
    );
    assert_eq!(
        fs::read_to_string(&setup.config).unwrap(),
        format!("# mine\nsloppy_focus = 0\nplugin = {path}, marker, false\n")
    );

    fs::remove_file(&setup.marker).unwrap();
    setup.plug(&compositor, &[path, "on"]);
    wait_until("the plugin to start again", || setup.marker.exists());
    setup.plug(&compositor, &["define", "marker"]);
    assert_eq!(
        setup.plug(&compositor, &["remove", path]),
        format!("removed {path}\n")
    );
    wait_until("the plugin to stop for good", || {
        !setup.running(&compositor)
    });
    assert_eq!(
        fs::read_to_string(&setup.config).unwrap(),
        "# mine\nsloppy_focus = 0\n"
    );
}

#[test]
fn wirbel_plug_refuses_what_cannot_run() {
    use std::os::unix::fs::PermissionsExt;
    let setup = PlugSetup::new("plug-refuse");
    let compositor = Compositor::start();
    fs::set_permissions(&setup.plugin, fs::Permissions::from_mode(0o644)).unwrap();
    let path = setup.plugin.to_str().unwrap();
    assert!(
        setup
            .plug_failing(&compositor, &["add", path])
            .contains("chmod +x")
    );
    assert!(
        setup
            .plug_failing(&compositor, &["add", "/does/not/exist"])
            .contains("No such file")
    );
    assert!(
        setup
            .plug_failing(&compositor, &["frobnicate"])
            .contains("usage")
    );
    assert_eq!(
        fs::read_to_string(&setup.config).unwrap(),
        "# mine\nsloppy_focus = 0\n"
    );
}

#[test]
fn wirbel_restart_ends_the_compositor_for_the_session_to_start_again() {
    let compositor = Compositor::start();
    let socket = compositor.ipc_path();
    assert_eq!(compositor.run(&["restart"]), "");
    // The loop stopped and took everything with it. What comes next is up to main or the script.
    wait_until("the compositor to stop", || !socket.exists());
    assert!(
        compositor
            .run_failing(&["restart", "now"])
            .contains("usage")
    );
}

#[test]
fn a_grab_takes_the_button_for_a_whole_session_until_the_modifiers_go_up() {
    let (compositor, mut client, _, _) = two_windows();
    let mut plugin = grabbing_plugin(&compositor);
    hold(&compositor, CTRL_ALT, KeyState::Pressed);
    client.sync();
    client.take_input();
    stroke(&compositor, (100.0, 100.0), (300.0, 300.0));
    stroke(&compositor, (300.0, 100.0), (100.0, 300.0));
    hold(&compositor, CTRL_ALT, KeyState::Released);

    let mut lines = vec![plugin.line()];
    while lines.last().unwrap().starts_with("event grab") && !lines.last().unwrap().contains("end")
    {
        lines.push(plugin.line());
    }
    assert_eq!(lines[0], "event grab press 100.0 100.0");
    assert_eq!(lines[1], "event grab motion 150.0 150.0");
    assert_eq!(lines[4], "event grab motion 300.0 300.0");
    assert_eq!(lines[5], "event grab release 300.0 300.0");
    assert_eq!(lines[6], "event grab motion 300.0 100.0");
    assert_eq!(lines[7], "event grab press 300.0 100.0");
    assert_eq!(lines[12], "event grab release 100.0 300.0");
    assert_eq!(lines.last().unwrap(), "event grab end 100.0 300.0");

    client.sync();
    let input = client.take_input();
    assert!(
        !input.iter().any(|input| matches!(input, Input::Button(..))),
        "{input:?}"
    );
    // Without the modifiers a click is a click.
    compositor.click();
    client.sync();
    assert!(
        client
            .take_input()
            .contains(&Input::Button(BTN_LEFT, wl_pointer::ButtonState::Pressed))
    );
}

#[test]
fn the_button_may_go_up_after_the_session() {
    let (compositor, mut client, _, _) = two_windows();
    let mut plugin = grabbing_plugin(&compositor);
    hold(&compositor, CTRL_ALT, KeyState::Pressed);
    compositor.pointer_at(100.0, 100.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    client.take_input();
    hold(&compositor, CTRL_ALT, KeyState::Released);
    compositor.button(BTN_LEFT, ButtonState::Released);
    assert_eq!(plugin.line(), "event grab press 100.0 100.0");
    assert_eq!(plugin.line(), "event grab end 100.0 100.0");
    client.sync();
    let input = client.take_input();
    assert!(
        !input.iter().any(|input| matches!(input, Input::Button(..))),
        "{input:?}"
    );
}

#[test]
fn escape_cancels_a_session_and_stays_with_wirbel() {
    let (compositor, mut client, _, _) = two_windows();
    let mut plugin = grabbing_plugin(&compositor);
    hold(&compositor, CTRL_ALT, KeyState::Pressed);
    compositor.pointer_at(100.0, 100.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    client.take_input();
    compositor.tap_key(KEY_ESC);
    compositor.button(BTN_LEFT, ButtonState::Released);
    hold(&compositor, CTRL_ALT, KeyState::Released);
    assert_eq!(plugin.line(), "event grab press 100.0 100.0");
    assert_eq!(plugin.line(), "event grab cancel");
    client.sync();
    let input = client.take_input();
    assert!(
        !input
            .iter()
            .any(|input| matches!(input, Input::Key(KEY_ESC, _) | Input::Button(..))),
        "{input:?}"
    );
}

#[test]
fn a_grab_ends_with_its_connection_and_belongs_to_one_plugin() {
    let (compositor, mut client, _, _) = two_windows();
    let plugin = grabbing_plugin(&compositor);
    let mut other = compositor.raw_ipc();
    assert!(other.request("grab ctrl+alt btn_left").starts_with("error"));
    assert!(other.request("grab none btn_left").starts_with("error"));
    assert_eq!(other.request("grab ctrl+alt btn_right"), "ok");
    assert_eq!(other.request("ungrab ctrl+alt btn_right"), "ok");
    drop(plugin);
    other.request("ungrab ctrl+alt btn_right");
    wait_until_plugin_input_unused(&compositor);
    compositor.pointer_at(200.0, 300.0);
    hold(&compositor, CTRL_ALT, KeyState::Pressed);
    client.sync();
    client.take_input();
    compositor.click();
    client.sync();
    assert!(
        client
            .take_input()
            .contains(&Input::Button(BTN_LEFT, wl_pointer::ButtonState::Pressed))
    );
    hold(&compositor, CTRL_ALT, KeyState::Released);
}

#[test]
fn plugins_draw_lines_that_fade_and_leave_with_them() {
    let (compositor, _client, _, _) = two_windows();
    let mut plugin = compositor.raw_ipc();
    assert_eq!(plugin.request("line 100 300 400 300 6 0x00ffff"), "ok");
    assert_eq!(compositor.screenshot().at(250, 300), 0x00_ffff);
    assert_eq!(compositor.screenshot().at(250, 310), 0xff_0000);
    assert_eq!(plugin.request("clear"), "ok");
    assert_eq!(compositor.screenshot().at(250, 300), 0xff_0000);

    assert_eq!(plugin.request("line 100 300 400 300 6 0x00ffff"), "ok");
    assert_eq!(plugin.request("clear 10000 0xffffff"), "ok");
    assert_eq!(
        compositor.screenshot().at(250, 300) >> 16,
        0xff,
        "fading in white"
    );
    drop(plugin);
    wait_until_plugin_input_unused(&compositor);
    let mut ipc = compositor.raw_ipc();
    assert_eq!(compositor.screenshot().at(250, 300), 0xff_0000);
    assert!(ipc.request("line 0 0 1 1 0 0xffffff").starts_with("error"));
}

#[test]
fn plugins_find_focus_and_close_the_window_at_a_place() {
    let (compositor, mut client, a, b) = two_windows();
    let mut plugin = compositor.raw_ipc();
    let left = plugin.request("window_at 200 300");
    let right = plugin.request(&format!("window_at {} 300", RIGHT_X + 50.0));
    assert_ne!(left, right);
    assert_eq!(plugin.request("window_at 500 2"), "none");
    assert_eq!(plugin.request(&format!("focus {left}")), "ok");
    client.sync();
    assert!(client.activated(a));
    assert_eq!(plugin.request(&format!("close {right}")), "ok");
    client.sync();
    assert_eq!(client.state.closed, [b]);
    assert!(plugin.request("close 99999").starts_with("error"));
}

#[test]
fn injection_needs_test_input() {
    use wirbel::inject::Inject;
    let compositor = Compositor::start();
    let answer = compositor.with(|state| state.inject(Inject::Wheel(1.0)));
    assert!(answer.is_err());
}

#[test]
fn injected_input_takes_the_real_paths() {
    use wirbel::inject::Inject;
    let (compositor, mut client, _, _) = two_windows();
    compositor.with(|state| state.test_input = true);
    let inject = |request: Inject| compositor.with(move |state| state.inject(request)).unwrap();
    let key = |code, pressed| {
        let state = if pressed {
            wl_keyboard::KeyState::Pressed
        } else {
            wl_keyboard::KeyState::Released
        };
        Input::Key(code, state)
    };

    // A capital letter takes Shift around it.
    inject(Inject::Type("aA".into()));
    assert_eq!(
        client.take_input(),
        [
            key(KEY_A, true),
            key(KEY_A, false),
            key(KEY_LEFTSHIFT, true),
            key(KEY_A, true),
            key(KEY_A, false),
            key(KEY_LEFTSHIFT, false),
        ]
    );
    let cannot = compositor.with(|state| state.inject(Inject::Type("a語".into())));
    assert!(cannot.is_err());
    assert_eq!(client.take_input(), [], "half a word got typed");

    // A chord runs the bind like the keyboard would.
    let chord = Inject::parse("inject chord SUPER+2").unwrap().unwrap();
    inject(chord);
    assert_eq!(compositor.active_workspace(), 1);
    inject(Inject::parse("inject chord SUPER+1").unwrap().unwrap());
    assert_eq!(compositor.active_workspace(), 0);

    // Relative motion moves the pointer like a mouse.
    compositor.pointer_at(100.0, 100.0);
    inject(Inject::Motion {
        dx: 50.0,
        dy: -20.0,
    });
    let location = compositor.with(|state| state.seat.get_pointer().unwrap().current_location());
    assert_eq!((location.x, location.y), (150.0, 80.0));

    client.take_input();
    inject(Inject::Button {
        button: BTN_RIGHT,
        down: true,
    });
    inject(Inject::Button {
        button: BTN_RIGHT,
        down: false,
    });
    inject(Inject::Wheel(2.0));
    let input = client.take_input();
    assert!(input.contains(&Input::Button(BTN_RIGHT, wl_pointer::ButtonState::Pressed)));
    assert!(input.contains(&Input::Button(BTN_RIGHT, wl_pointer::ButtonState::Released)));
    assert!(input.contains(&Input::AxisV120(wl_pointer::Axis::VerticalScroll, 240)));
}

/// An overlay that draws nothing.
struct NoOverlay;

impl wirbel::scene_override::SceneOverride for NoOverlay {
    fn draw(
        &self,
        _gl: &smithay::backend::renderer::gles::ffi::Gles2,
        _target: &wirbel::scene_override::OverrideTarget,
    ) -> Result<(), String> {
        Ok(())
    }

    fn failed(&self, _error: &str) {}
}

#[test]
fn an_overlay_is_drawn_in_front_of_the_scene_which_stays() {
    use wirbel::scene_override::{OutputElement, SceneOverrideSlot};
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(0xff0000);
    client.sync();
    let drawn = || {
        compositor.with(|state| {
            state.space.refresh();
            let output = state.monitors[0].output.clone();
            let mut renderer = PixmanRenderer::new().unwrap();
            let elements = state.output_elements(&mut renderer, &output);
            let first_is_overlay = matches!(
                elements.first(),
                Some(OutputElement::Override(element)) if !element.is_release()
            );
            let scene = elements
                .iter()
                .filter(|element| matches!(element, OutputElement::Scene(_)))
                .count();
            (first_is_overlay, scene)
        })
    };
    let (overlay, scene) = drawn();
    assert!(!overlay);
    assert!(scene > 0);
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        let slot = SceneOverrideSlot::overlay(output, std::rc::Rc::new(NoOverlay));
        assert!(slot.is_overlay());
        state.set_overlay(Some(slot));
    });
    let (overlay, with) = drawn();
    assert!(overlay);
    assert_eq!(with, scene, "the scene is drawn behind it as before");
    // Taken off, it is released once and the scene alone is left.
    compositor.with(|state| state.set_overlay(None));
    compositor.with(|state| {
        assert!(state.overlay.is_none());
        assert_eq!(state.retired_overrides.len(), 1);
    });
}
