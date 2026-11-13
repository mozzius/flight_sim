//! The native sign-in prompt (the web's is `web/auth.js`'s HTML form): one
//! line, a handle field and SIGN IN / CANCEL, over the mode menu.
//!
//! The field is Bevy's `TextInput` (typing, backspace, arrows, paste with the
//! system clipboard). While the prompt is open the keyboard belongs to it:
//! `ButtonInput<KeyCode>` is cleared so the menu and the game's own keys
//! (M, N...) don't react to typing; Enter signs in, Esc closes. A gamepad
//! moves between the buttons as usual (B closes).
//!
//! SIGN IN hands the handle to `oauth::begin`, which opens the system
//! browser; the button reads `...` until the browser comes back (CANCEL gives
//! up). Errors show for a few seconds in one line.
//!
//! Dev: `FLIGHT_SIM_DEV_SIGN_IN=<handle>|entryway` starts signing in at
//! launch, and `FLIGHT_SIM_OAUTH_PRINT_URL=1` logs the authorization URL
//! instead of opening the browser; `FLIGHT_SIM_MODE=menu-signin` shows the
//! prompt in a screenshot run.

use bevy::{
    input::{InputSystems, keyboard::KeyboardInput},
    input_focus::{FocusCause, InputFocus},
    picking::hover::Hovered,
    prelude::*,
    text::{EditableText, TextCursorStyle},
    ui_widgets::TextInput,
};

use crate::{
    loading::GameState,
    menu::{BG, BORDER, BUTTON, DIM, GREEN, Pressed, Screen, UiAction, UiButton, font_of},
};

pub struct SignInPlugin;

impl Plugin for SignInPlugin {
    fn build(&self, app: &mut App) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            crate::oauth::init();
            // dev: start signing in at once (`entryway` for an empty handle);
            // with FLIGHT_SIM_OAUTH_PRINT_URL=1 the URL is logged, not opened
            if let Some(h) = crate::environment::setting("FLIGHT_SIM_DEV_SIGN_IN") {
                crate::oauth::begin(if h == "entryway" { "" } else { &h });
            }
        }
        // dev: the prompt open at launch (screenshots of it)
        let open = crate::environment::setting("FLIGHT_SIM_DEV_SIGN_IN").is_some()
            || crate::environment::setting("FLIGHT_SIM_MODE").is_some_and(|m| m.eq_ignore_ascii_case("menu-signin"));
        app.insert_resource(SignInPrompt { open, fresh: false })
            .add_systems(PreUpdate, swallow_keys.after(InputSystems))
            .add_systems(OnExit(GameState::Menu), close)
            .add_systems(Update, (keys, sync).chain().after(crate::menu::MenuSystems));
    }
}

/// Whether the prompt is up (the menu hands it the focus).
#[derive(Resource, Default)]
pub struct SignInPrompt {
    pub open: bool,
    /// Opened this frame (the Enter that opened it mustn't submit it).
    fresh: bool,
}

impl SignInPrompt {
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn show(&mut self) {
        if !self.open {
            self.open = true;
            self.fresh = true;
        }
    }
}

/// SIGN IN in the menu: the game's prompt natively, the page's on the web.
pub fn sign_in(prompt: &mut SignInPrompt) {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = prompt;
        crate::identity::sign_in();
    }
    #[cfg(not(target_arch = "wasm32"))]
    prompt.show();
}

/// SIGN IN in the prompt (from `menu::handle_pressed`, like CANCEL).
pub fn submit(field: &str) {
    #[cfg(not(target_arch = "wasm32"))]
    crate::oauth::begin(field);
    #[cfg(target_arch = "wasm32")]
    let _ = field;
}

/// CANCEL: stops waiting for the browser, else closes the prompt.
pub fn cancel(prompt: &mut SignInPrompt) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        if crate::oauth::status() == crate::oauth::Status::Waiting {
            crate::oauth::cancel();
            return;
        }
    }
    prompt.open = false;
}

#[derive(Component)]
struct PromptRoot;

#[derive(Component)]
pub struct HandleField;

#[derive(Component)]
struct Placeholder;

#[derive(Component)]
struct SubmitLabel;

#[derive(Component)]
struct ErrorLine;

fn spawn(commands: &mut Commands, asset_server: &AssetServer) -> Entity {
    let font: Handle<Font> = asset_server.load("fonts/RobotoMono/RobotoMono-Medium.ttf");
    let mut field = Entity::PLACEHOLDER;
    commands
        .spawn((
            PromptRoot,
            crate::intro::KeepDuringIntro,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                row_gap: px(16),
                ..default()
            },
            BackgroundColor(BG),
            GlobalZIndex(30),
        ))
        .with_children(|root| {
            root.spawn((Text::new("Sign in with your Atmosphere account (e.g. Bluesky)"), font_of(&font, 15.0, DIM)));
            root.spawn((
                Node {
                    width: px(374),
                    padding: UiRect::axes(px(14), px(10)),
                    border: UiRect::all(px(1)),
                    border_radius: BorderRadius::all(px(6)),
                    ..default()
                },
                BorderColor::all(BORDER),
                BackgroundColor(BUTTON),
            ))
            .with_children(|b| {
                b.spawn((
                    Placeholder,
                    Text::new("handle.bsky.social"),
                    font_of(&font, 18.0, DIM.with_alpha(0.5)),
                    Node { position_type: PositionType::Absolute, left: px(14), top: px(10), ..default() },
                    Pickable::IGNORE,
                ));
                field = b
                    .spawn((
                        HandleField,
                        TextInput,
                        EditableText { max_characters: Some(253), allow_newlines: false, ..default() },
                        TextLayout::no_wrap(),
                        font_of(&font, 18.0, GREEN),
                        TextCursorStyle { color: GREEN, selection_color: BORDER, ..default() },
                        Node { width: percent(100), ..default() },
                    ))
                    .id();
            });
            root.spawn((ErrorLine, Text::new(""), font_of(&font, 14.0, Color::srgb(1.0, 0.55, 0.45))));
            root.spawn(Node { column_gap: px(14), ..default() }).with_children(|row| {
                for (label, order, action) in [("SIGN IN", 0, UiAction::SignInSubmit), ("CANCEL", 1, UiAction::SignInCancel)] {
                    let mut b = row.spawn((
                        UiButton { screen: Screen::SignIn, order, row: 0, action },
                        Hovered::default(),
                        Node {
                            min_width: px(180),
                            min_height: px(48),
                            padding: UiRect::axes(px(24), px(10)),
                            border: UiRect::all(px(2)),
                            border_radius: BorderRadius::all(px(6)),
                            justify_content: JustifyContent::Center,
                            align_items: AlignItems::Center,
                            ..default()
                        },
                        BorderColor::all(BORDER),
                        BackgroundColor(BUTTON),
                    ));
                    b.observe(move |_: On<PointerClick>, mut pressed: ResMut<Pressed>| pressed.0.push((action, 1)));
                    b.with_children(|b| {
                        let mut text = b.spawn((Text::new(label), font_of(&font, 20.0, GREEN), Pickable::IGNORE));
                        if action == UiAction::SignInSubmit {
                            text.insert(SubmitLabel);
                        }
                    });
                }
            });
        });
    field
}

/// While the prompt is up, the game doesn't see the keyboard (the field
/// still does: it reads `KeyboardInput` messages).
fn swallow_keys(prompt: Res<SignInPrompt>, mut keys: ResMut<ButtonInput<KeyCode>>) {
    if prompt.open {
        keys.reset_all();
    }
}

/// Enter signs in, Esc closes.
fn keys(
    mut prompt: ResMut<SignInPrompt>,
    mut input: MessageReader<KeyboardInput>,
    mut pressed: ResMut<Pressed>,
) {
    let fresh = std::mem::take(&mut prompt.fresh);
    if !prompt.open || fresh {
        input.clear();
        return;
    }
    for k in input.read() {
        if !k.state.is_pressed() || k.repeat {
            continue;
        }
        match k.key_code {
            KeyCode::Enter | KeyCode::NumpadEnter => pressed.0.push((UiAction::SignInSubmit, 1)),
            KeyCode::Escape => pressed.0.push((UiAction::SignInCancel, 1)),
            _ => {}
        }
    }
}

/// Spawns / despawns the prompt, keeps the field focused and the labels current.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn sync(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut prompt: ResMut<SignInPrompt>,
    state: Res<State<GameState>>,
    roots: Query<Entity, With<PromptRoot>>,
    mut focus: ResMut<InputFocus>,
    fields: Query<(Entity, &EditableText), With<HandleField>>,
    mut placeholder: Query<&mut Visibility, With<Placeholder>>,
    mut submit: Query<&mut Text, (With<SubmitLabel>, Without<ErrorLine>)>,
    mut error: Query<&mut Text, (With<ErrorLine>, Without<SubmitLabel>)>,
) {
    // (signed in: done; leaving the menu closes it, see `close`)
    if prompt.open && crate::identity::current_user().is_some() {
        prompt.open = false;
    }
    if !prompt.open || *state.get() != GameState::Menu {
        for e in &roots {
            commands.entity(e).despawn();
            focus.clear();
        }
        return;
    }
    if roots.is_empty() {
        let field = spawn(&mut commands, &asset_server);
        focus.set(field, FocusCause::Auto);
        return;
    }
    #[cfg(not(target_arch = "wasm32"))]
    let status = crate::oauth::status();
    #[cfg(not(target_arch = "wasm32"))]
    let (waiting, err) = match &status {
        crate::oauth::Status::Waiting => (true, String::new()),
        crate::oauth::Status::Failed(e) => (false, e.to_ascii_uppercase()),
        crate::oauth::Status::Idle => (false, String::new()),
    };
    #[cfg(target_arch = "wasm32")]
    let (waiting, err) = (false, String::new());
    if let Ok((field, text)) = fields.single() {
        if focus.get().is_none() && !waiting {
            focus.set(field, FocusCause::Auto);
        }
        let empty = text.value().to_string().is_empty();
        if let Ok(mut v) = placeholder.single_mut() {
            let want = if empty { Visibility::Inherited } else { Visibility::Hidden };
            if *v != want {
                *v = want;
            }
        }
    }
    if let Ok(mut t) = submit.single_mut() {
        let l = if waiting { "..." } else { "SIGN IN" };
        if t.0 != l {
            t.0 = l.into();
        }
    }
    if let Ok(mut t) = error.single_mut()
        && t.0 != err
    {
        t.0 = err;
    }
}

fn close(mut prompt: ResMut<SignInPrompt>) {
    prompt.open = false;
    #[cfg(not(target_arch = "wasm32"))]
    crate::oauth::cancel();
}

/// The handle typed so far.
pub fn typed(fields: &Query<&EditableText, With<HandleField>>) -> String {
    fields.single().map(|t| t.value().to_string()).unwrap_or_default()
}
