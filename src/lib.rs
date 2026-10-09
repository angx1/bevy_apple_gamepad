//! Gamepad support for [Bevy](https://bevy.org) on macOS through Apple's
//! [Game Controller framework](https://developer.apple.com/documentation/gamecontroller).
//!
//! # Why
//!
//! Bevy reads gamepads with [gilrs](https://gitlab.com/gilrs-project/gilrs), which on macOS uses
//! IOKit HID. Since macOS 14, controllers managed by the Game Controller framework (e.g. Xbox
//! controllers) no longer deliver input through IOKit: Bevy reports the gamepad as connected but
//! no button or stick ever changes. Apple offers a compatibility layer ("Increase controller
//! compatibility"), but gilrs skips the synthetic device it creates because it has no
//! `LocationID`.
//!
//! This plugin reads controllers with the Game Controller framework instead (the API Apple
//! recommends, and what browsers use) and feeds them into Bevy's **standard** gamepad pipeline by
//! writing the same raw events `bevy_gilrs` writes. Everything else keeps working unchanged:
//! the [`Gamepad`](bevy_input::gamepad::Gamepad) component, `GamepadSettings` dead zones,
//! `just_pressed`, etc.
//!
//! # Usage
//!
//! ```no_run
//! use bevy::prelude::*;
//! use bevy_apple_gamepad::AppleGamepadPlugin;
//!
//! fn main() {
//!     let mut app = App::new();
//!     // On macOS, replace gilrs (otherwise you get a second, silent gamepad).
//!     #[cfg(target_os = "macos")]
//!     app.add_plugins(DefaultPlugins.build().disable::<bevy::gilrs::GilrsPlugin>());
//!     #[cfg(not(target_os = "macos"))]
//!     app.add_plugins(DefaultPlugins);
//!     app.add_plugins(AppleGamepadPlugin::default()).run();
//! }
//! ```
//!
//! On other platforms the plugin does nothing, so it's safe to add unconditionally.
//!
//! # Button mapping
//!
//! Bevy names buttons by position. Using Xbox labels:
//!
//! | Game Controller        | Bevy `GamepadButton` |
//! |------------------------|----------------------|
//! | A / B / X / Y          | `South` / `East` / `West` / `North` |
//! | LB / RB                | `LeftTrigger` / `RightTrigger`      |
//! | LT / RT (analog)       | `LeftTrigger2` / `RightTrigger2`    |
//! | View / Menu / Xbox     | `Select` / `Start` / `Mode`         |
//! | Stick clicks           | `LeftThumb` / `RightThumb`          |
//! | D-pad                  | `DPadUp` / `DPadDown` / `DPadLeft` / `DPadRight` |
//!
//! Sticks map to `LeftStickX/Y` and `RightStickX/Y` (up is positive Y).

use bevy_app::{App, Plugin};

/// Adds gamepad support through Apple's Game Controller framework (macOS only; no-op elsewhere).
#[derive(Debug, Clone, Default)]
pub struct AppleGamepadPlugin {
    /// Keep receiving controller input while the game window is not focused.
    /// By default macOS only delivers controller input to the foreground app.
    pub monitor_background_events: bool,
}

impl Plugin for AppleGamepadPlugin {
    #[cfg(target_os = "macos")]
    fn build(&self, app: &mut App) {
        macos::build(self, app);
    }

    #[cfg(not(target_os = "macos"))]
    fn build(&self, _app: &mut App) {}
}

#[cfg(target_os = "macos")]
mod macos {
    use bevy_app::{App, PreUpdate};
    use bevy_ecs::{prelude::*, system::NonSendMarker};
    use bevy_input::{
        InputSystems,
        gamepad::{
            GamepadAxis, GamepadButton, GamepadConnection, GamepadConnectionEvent,
            RawGamepadAxisChangedEvent, RawGamepadButtonChangedEvent, RawGamepadEvent,
        },
    };
    use bevy_log::info;
    use bevy_platform::collections::HashMap;
    use objc2::rc::Retained;
    use objc2_game_controller::{
        GCController, GCControllerButtonInput, GCDevice, GCExtendedGamepad,
    };

    use super::AppleGamepadPlugin;

    pub(super) fn build(plugin: &AppleGamepadPlugin, app: &mut App) {
        // SAFETY: plain class property setter, called during app setup on the main thread.
        unsafe {
            GCController::setShouldMonitorBackgroundEvents(plugin.monitor_background_events);
        }
        app.init_resource::<ConnectedControllers>().add_systems(
            PreUpdate,
            // Same place as bevy_gilrs: raw events must be written before Bevy processes them.
            poll_controllers.before(InputSystems),
        );
    }

    /// Last known state of each connected controller, keyed by the controller object's address
    /// (the framework keeps the same `GCController` instance for as long as it stays connected).
    #[derive(Resource, Default)]
    struct ConnectedControllers(HashMap<usize, TrackedController>);

    struct TrackedController {
        entity: Entity,
        buttons: [f32; BUTTONS.len()],
        axes: [f32; AXES.len()],
    }

    type ButtonGetter = fn(&GCExtendedGamepad) -> Option<Retained<GCControllerButtonInput>>;
    type AxisGetter = fn(&GCExtendedGamepad) -> f32;

    // SAFETY (all getters below): read-only property accessors of a live `GCExtendedGamepad`,
    // called from the main thread (the polling system takes `NonSendMarker`).
    const BUTTONS: [(GamepadButton, ButtonGetter); 17] = [
        (GamepadButton::South, |g| Some(unsafe { g.buttonA() })),
        (GamepadButton::East, |g| Some(unsafe { g.buttonB() })),
        (GamepadButton::West, |g| Some(unsafe { g.buttonX() })),
        (GamepadButton::North, |g| Some(unsafe { g.buttonY() })),
        (GamepadButton::LeftTrigger, |g| Some(unsafe { g.leftShoulder() })),
        (GamepadButton::RightTrigger, |g| Some(unsafe { g.rightShoulder() })),
        (GamepadButton::LeftTrigger2, |g| Some(unsafe { g.leftTrigger() })),
        (GamepadButton::RightTrigger2, |g| Some(unsafe { g.rightTrigger() })),
        (GamepadButton::Select, |g| unsafe { g.buttonOptions() }),
        (GamepadButton::Start, |g| Some(unsafe { g.buttonMenu() })),
        (GamepadButton::Mode, |g| unsafe { g.buttonHome() }),
        (GamepadButton::LeftThumb, |g| unsafe { g.leftThumbstickButton() }),
        (GamepadButton::RightThumb, |g| unsafe { g.rightThumbstickButton() }),
        (GamepadButton::DPadUp, |g| Some(unsafe { g.dpad().up() })),
        (GamepadButton::DPadDown, |g| Some(unsafe { g.dpad().down() })),
        (GamepadButton::DPadLeft, |g| Some(unsafe { g.dpad().left() })),
        (GamepadButton::DPadRight, |g| Some(unsafe { g.dpad().right() })),
    ];

    const AXES: [(GamepadAxis, AxisGetter); 4] = [
        (GamepadAxis::LeftStickX, |g| unsafe { g.leftThumbstick().xAxis().value() }),
        (GamepadAxis::LeftStickY, |g| unsafe { g.leftThumbstick().yAxis().value() }),
        (GamepadAxis::RightStickX, |g| unsafe { g.rightThumbstick().xAxis().value() }),
        (GamepadAxis::RightStickY, |g| unsafe { g.rightThumbstick().yAxis().value() }),
    ];

    /// Polls every connected controller once per frame and writes Bevy's raw gamepad events for
    /// connections, disconnections and changed values (mirrors `bevy_gilrs::gilrs_event_system`).
    #[allow(clippy::too_many_arguments)]
    fn poll_controllers(
        _main_thread: NonSendMarker,
        mut commands: Commands,
        mut tracked: ResMut<ConnectedControllers>,
        mut events: MessageWriter<RawGamepadEvent>,
        mut connection_events: MessageWriter<GamepadConnectionEvent>,
        mut button_events: MessageWriter<RawGamepadButtonChangedEvent>,
        mut axis_events: MessageWriter<RawGamepadAxisChangedEvent>,
    ) {
        // SAFETY: class method returning a snapshot array of connected controllers.
        let controllers = unsafe { GCController::controllers() };
        let mut seen = Vec::with_capacity(controllers.len());

        for controller in controllers.iter() {
            // Only controllers with the standard (extended) layout: Xbox, PlayStation, Switch Pro…
            // SAFETY: read-only property of a live controller.
            let Some(gamepad) = (unsafe { controller.extendedGamepad() }) else {
                continue;
            };
            let key = Retained::as_ptr(&controller) as usize;
            seen.push(key);

            let state = tracked.0.entry(key).or_insert_with(|| {
                let entity = commands.spawn_empty().id();
                // SAFETY: read-only property of a live controller.
                let name = unsafe { controller.vendorName() }
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "Game Controller".to_string());
                info!("Game Controller framework: connected {name:?} as {entity}");
                let event = GamepadConnectionEvent::new(
                    entity,
                    GamepadConnection::Connected {
                        name,
                        // The framework doesn't expose USB vendor/product ids.
                        vendor_id: None,
                        product_id: None,
                    },
                );
                events.write(event.clone().into());
                connection_events.write(event);
                TrackedController {
                    entity,
                    buttons: [0.0; BUTTONS.len()],
                    axes: [0.0; AXES.len()],
                }
            });

            for (i, (button, get)) in BUTTONS.iter().enumerate() {
                // SAFETY: read-only property of a live button input.
                let value = get(&gamepad).map_or(0.0, |input| unsafe { input.value() });
                if value != state.buttons[i] {
                    state.buttons[i] = value;
                    let event = RawGamepadButtonChangedEvent::new(state.entity, *button, value);
                    events.write(event.clone().into());
                    button_events.write(event);
                }
            }
            for (i, (axis, get)) in AXES.iter().enumerate() {
                let value = get(&gamepad);
                if value != state.axes[i] {
                    state.axes[i] = value;
                    let event = RawGamepadAxisChangedEvent::new(state.entity, *axis, value);
                    events.write(event.clone().into());
                    axis_events.write(event);
                }
            }
        }

        // Controllers that are no longer in the list were disconnected.
        tracked.0.retain(|key, state| {
            let connected = seen.contains(key);
            if !connected {
                info!("Game Controller framework: disconnected {}", state.entity);
                let event =
                    GamepadConnectionEvent::new(state.entity, GamepadConnection::Disconnected);
                events.write(event.clone().into());
                connection_events.write(event);
            }
            connected
        });
    }
}
